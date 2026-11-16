//! The second copy of the backups (in French, *la copie de secours*): a
//! place its owner chooses from the console, the API or the command line,
//! besides the one the environment names, or as the only one when the
//! environment names none.
//!
//! Each run of the server writes a complete backup of its own there, in a
//! folder of its own (`lives/<start>-<id>/`), and never writes into another
//! run's. So nothing there is ever overwritten, whoever else uses the same
//! place: the server before its last restart, or a copy of this server
//! restored somewhere else, whose saved settings name the same place. The
//! last [`KEPT`] complete runs are kept; older ones go when newer ones are
//! complete.
//!
//! The server never restores from it by itself: a start on an empty disk
//! restores from the place the environment names, the only one it knows
//! before it has its data. To start from a second copy, the folder of a run
//! is named in the environment (the Settings page gives the lines), and the
//! server restores from it as from any backup.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::settings::Refusal;
use crate::engine::{BackupStore, DirectoryStore, Engine, PrefixedStore, storage_err};

/// How many complete runs a place keeps, the one being written aside.
pub(crate) const KEPT: usize = 3;

/// Where the runs are, in a place.
const LIVES: &str = "lives/";

/// The sentences of the refusals (each has its French and Spanish in the
/// console's dictionary: a test checks it).
pub(crate) mod refused {
    pub(crate) const KIND: &str = "the second copy is a folder (`dir`) or S3-compatible storage (`s3`)";
    pub(crate) const PATH: &str =
        "`{}` is not the full path of a folder: write it from the root (`/mnt/backup/nuage`, `E:\\Backup\\nuage`)";
    pub(crate) const PARENT: &str = "the folder that would hold `{}` does not exist: a disk that is not plugged in, or a mistyped path (only the last folder is made for you)";
    pub(crate) const IN_DATA: &str =
        "`{}` is inside the data directory: a copy on the disk of the data is lost with it, choose another disk";
    pub(crate) const S3_MISSING: &str = "S3-compatible storage needs `{}`";
    pub(crate) const SAME: &str = "`{}` is where the backups already go: a second copy is somewhere else";
    pub(crate) const PROBE: &str = "nothing could be written to {}: {}";
    pub(crate) const NOT_FOUND_AGAIN: &str = "on this host the disk is erased at each restart, and the server could not find this place again by itself: name the destination of the backups in the environment (NUAGE_BACKUP_S3_*); a second copy can be added here once there is one";
    #[cfg_attr(feature = "s3", allow(dead_code))]
    pub(crate) const NO_S3: &str = "this build of Nuage DB has no `s3` feature: the second copy is a folder";

    #[cfg(test)]
    pub(crate) const ALL: [&str; 9] = [KIND, PATH, PARENT, IN_DATA, S3_MISSING, SAME, PROBE, NOT_FOUND_AGAIN, NO_S3];
}

/// Where a second copy goes.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Place {
    /// A folder: another disk, a network share.
    Dir(PathBuf),
    /// S3-compatible storage.
    S3 {
        endpoint: String,
        bucket: String,
        region: String,
        prefix: String,
        access_key_id: String,
        /// Kept with the settings (encrypted with the data), never shown again.
        secret_access_key: String,
    },
}

impl std::fmt::Debug for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret.
        f.write_str(&self.describe())
    }
}

impl Place {
    /// A place as its owner describes it: `{"kind": "dir", "path": …}`, or
    /// `{"kind": "s3", "bucket", "access_key_id", "secret_access_key",
    /// "region"?, "endpoint"?, "prefix"?}`. A secret left empty keeps the one
    /// of `before` when the rest names the same storage.
    pub(crate) fn from_json(body: &Json, before: Option<&Place>) -> std::result::Result<Place, Refusal> {
        let text = |key: &str| body.get(key).and_then(Json::as_str).map(str::trim).unwrap_or_default().to_owned();
        match text("kind").as_str() {
            "dir" => {
                let path = PathBuf::from(text("path"));
                if !path.is_absolute() {
                    return Err(Refusal::new(refused::PATH, &[&path.display()]));
                }
                Ok(Place::Dir(path))
            }
            "s3" => {
                let (bucket, access_key_id) = (text("bucket"), text("access_key_id"));
                for (name, value) in [("bucket", &bucket), ("access_key_id", &access_key_id)] {
                    if value.is_empty() {
                        return Err(Refusal::new(refused::S3_MISSING, &[&name]));
                    }
                }
                let region = Some(text("region")).filter(|r| !r.is_empty()).unwrap_or_else(|| "us-east-1".into());
                let endpoint = Some(text("endpoint"))
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com"));
                let mut prefix = Some(text("prefix")).filter(|p| !p.is_empty()).unwrap_or_else(|| "nuage/".into());
                if !prefix.ends_with('/') {
                    prefix.push('/');
                }
                let mut secret_access_key = text("secret_access_key");
                if secret_access_key.is_empty()
                    && let Some(Place::S3 { endpoint: e, bucket: b, access_key_id: k, secret_access_key: s, .. }) =
                        before
                    && (e, b, k) == (&endpoint, &bucket, &access_key_id)
                {
                    secret_access_key = s.clone();
                }
                if secret_access_key.is_empty() {
                    return Err(Refusal::new(refused::S3_MISSING, &[&"secret_access_key"]));
                }
                Ok(Place::S3 { endpoint, bucket, region, prefix, access_key_id, secret_access_key })
            }
            _ => Err(Refusal::new(refused::KIND, &[])),
        }
    }

    /// The place as it is kept with the settings, its secret included.
    pub(crate) fn to_json(&self) -> Json {
        match self {
            Place::Dir(path) => json!({ "kind": "dir", "path": path.display().to_string() }),
            Place::S3 { endpoint, bucket, region, prefix, access_key_id, secret_access_key } => json!({
                "kind": "s3",
                "endpoint": endpoint,
                "bucket": bucket,
                "region": region,
                "prefix": prefix,
                "access_key_id": access_key_id,
                "secret_access_key": secret_access_key,
            }),
        }
    }

    /// The place as the API and the page show it: never its secret.
    pub(crate) fn shown(&self) -> Json {
        let mut shown = self.to_json();
        if let Some(object) = shown.as_object_mut() {
            object.remove("secret_access_key");
        }
        shown
    }

    /// Where it is, in one line: `/mnt/backup/nuage`, `s3://bucket/prefix (endpoint)`.
    pub(crate) fn describe(&self) -> String {
        match self {
            Place::Dir(path) => path.display().to_string(),
            Place::S3 { endpoint, bucket, prefix, .. } => format!("s3://{bucket}/{prefix} ({endpoint})"),
        }
    }

    /// Checks a folder before it is saved: its parent is there (only the
    /// last folder is made: an external disk that is not plugged in does not
    /// get its folder on the computer's own disk), and it is not within the
    /// data directory.
    pub(crate) fn check_folder(&self, data_dir: &Path) -> std::result::Result<(), Refusal> {
        let Place::Dir(path) = self else { return Ok(()) };
        let parent = path.parent().unwrap_or(path);
        if !parent.is_dir() {
            return Err(Refusal::new(refused::PARENT, &[&path.display()]));
        }
        let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let (data, parent) = (real(data_dir), real(parent));
        let place = parent.join(path.file_name().unwrap_or_default());
        if place.starts_with(&data) {
            return Err(Refusal::new(refused::IN_DATA, &[&path.display()]));
        }
        std::fs::create_dir(path)
            .or_else(|e| if path.is_dir() { Ok(()) } else { Err(e) })
            .map_err(|e| Refusal::new(refused::PROBE, &[&path.display(), &e]))
    }

    /// Removes the run `life` from the place, whole.
    fn remove_life(&self, root: &dyn BackupStore, life: &str) -> Result<()> {
        if !is_life(life) {
            return Err(Error::Validation(format!("`{life}` is not the folder of a run")));
        }
        match self {
            Place::Dir(path) => match std::fs::remove_dir_all(path.join(LIVES).join(life)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(storage_err(format!("cannot remove {life} from {}: {e}", path.display()))),
            },
            // Object storage lists every object under the run.
            Place::S3 { .. } => {
                for name in root.list(&format!("{LIVES}{life}/"))? {
                    root.delete(&name)?;
                }
                Ok(())
            }
        }
    }

    /// The store of the place.
    pub(crate) fn store(&self) -> std::result::Result<Arc<dyn BackupStore>, Refusal> {
        match self {
            Place::Dir(path) => Ok(Arc::new(PresentDir { root: path.clone(), inner: DirectoryStore::new(path) })),
            #[cfg(feature = "s3")]
            Place::S3 { endpoint, bucket, region, prefix, access_key_id, secret_access_key } => {
                let store = crate::s3::S3Store::new(crate::s3::S3Options {
                    endpoint: endpoint.clone(),
                    bucket: bucket.clone(),
                    region: region.clone(),
                    prefix: prefix.clone(),
                    access_key_id: access_key_id.clone(),
                    secret_access_key: secret_access_key.clone(),
                })
                .map_err(|e| Refusal::new(refused::PROBE, &[&self.describe(), &e]))?;
                Ok(Arc::new(store))
            }
            #[cfg(not(feature = "s3"))]
            Place::S3 { .. } => Err(Refusal::new(refused::NO_S3, &[])),
        }
    }
}

/// A folder that must be there to be written to: with its disk unplugged,
/// writing fails instead of making the folder again on the computer's own
/// disk, where a copy protects nothing.
struct PresentDir {
    root: PathBuf,
    inner: DirectoryStore,
}

impl PresentDir {
    fn present(&self) -> Result<()> {
        if self.root.is_dir() {
            Ok(())
        } else {
            Err(storage_err(format!(
                "the folder {} is not there (a disk that is not plugged in?): nothing is written in its place",
                self.root.display()
            )))
        }
    }
}

impl BackupStore for PresentDir {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, name: &str, data: &[u8]) -> Result<()> {
        self.present()?;
        self.inner.put(name, data)
    }
    fn put_file(&self, name: &str, path: &Path) -> Result<()> {
        self.present()?;
        self.inner.put_file(name, path)
    }
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(name)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        self.inner.list(prefix)
    }
    fn delete(&self, name: &str) -> Result<()> {
        self.inner.delete(name)
    }
}

/// Writes, reads back and removes a small object: the place takes what the
/// backups will need of it.
pub(crate) fn probe(root: &dyn BackupStore) -> std::result::Result<(), Refusal> {
    let name = format!("nuage-probe-{}", &super::random_token()[..12]);
    let written = b"Nuage DB: a place for backups";
    let tried = (|| -> Result<()> {
        root.put(&name, written)?;
        let read = root.get(&name)?;
        root.delete(&name)?;
        if read.as_deref() != Some(written.as_slice()) {
            return Err(Error::Validation("what was written there does not read back".into()));
        }
        Ok(())
    })();
    tried.map_err(|e| Refusal::new(refused::PROBE, &[&root.describe(), &e]))
}

/// The folder of a run: when it started (UTC), and a random id.
/// The names sort in the order of the starts.
pub(crate) fn life_name(started_ms: i64, id: &str) -> String {
    let at = chrono::DateTime::from_timestamp_millis(started_ms).unwrap_or_default();
    format!("{}-{}", at.format("%Y-%m-%dT%H-%M-%SZ"), &id[..id.len().min(6)])
}

/// The store of the run `life` in the place `root`: a backup location like
/// any other (the first application at its root, the others under `apps/`).
pub(crate) fn life_store(root: Arc<dyn BackupStore>, life: &str) -> Arc<dyn BackupStore> {
    Arc::new(PrefixedStore::new(root, format!("{LIVES}{life}/")))
}

/// A name that [`life_name`] makes: digits, letters and `-`.
fn is_life(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The runs a place holds, oldest first.
pub(crate) fn lives(root: &dyn BackupStore) -> Result<Vec<String>> {
    // A folder lists the runs themselves, object storage every object under them.
    let names: BTreeSet<String> = root
        .list(LIVES)?
        .iter()
        .filter_map(|name| name.strip_prefix(LIVES)?.split('/').next().map(str::to_owned))
        .filter(|life| is_life(life))
        .collect();
    Ok(names.into_iter().collect())
}

/// The run holds a complete backup of the first application (its manifest
/// is written last).
pub(crate) fn complete(root: &dyn BackupStore, life: &str) -> bool {
    root.get(&format!("{LIVES}{life}/manifest.json")).is_ok_and(|m| m.is_some())
}

/// Removes from `place` (whose store is `root`) the runs older than the
/// [`KEPT`] newest complete ones. The run `mine`, being written, is never one
/// of them, whatever its state: there are always that many complete backups
/// besides it. Returns the runs removed.
pub(crate) fn prune(place: &Place, root: &dyn BackupStore, mine: &str) -> Result<Vec<String>> {
    let others: Vec<String> = lives(root)?.into_iter().filter(|life| life != mine).collect();
    let complete_ones: Vec<&String> = others.iter().filter(|life| complete(root, life)).collect();
    let Some(oldest_kept) = complete_ones.len().checked_sub(KEPT).map(|i| complete_ones[i].clone()) else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for life in others.iter().filter(|life| **life < oldest_kept) {
        place.remove_life(root, life)?;
        removed.push(life.clone());
    }
    Ok(removed)
}

/// Where the second copy is kept: with the settings, in the first
/// database's metadata (encrypted with the data), its secret included.
pub(crate) const SAVED: &[u8] = b"setting:backup.second";

/// A complete copy of every database goes to the second copy this often,
/// besides one when the server starts and one when the place is saved.
pub(crate) const EVERY_MS: i64 = 60 * 60 * 1000;
/// After a failure, the next try waits this long, then twice as long each
/// time, up to [`RETRY_MOST_MS`].
const RETRY_FIRST_MS: i64 = 60 * 1000;
const RETRY_MOST_MS: i64 = 15 * 60 * 1000;

/// How the second copy goes, for the page, the API and the command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CopyState {
    /// When the last complete copy ended (Unix ms), in this run.
    pub last_at: Option<i64>,
    /// Why the last try failed, if it did.
    pub last_error: Option<String>,
    /// Since when the tries fail.
    pub failing_since: Option<i64>,
    /// When the next copy starts (Unix ms; 0: as soon as it can).
    pub next_at: i64,
    /// A copy is being written.
    pub running: bool,
    tries: u32,
}

/// The second copy of a server: where it goes, the folder of this run, how
/// it goes.
pub(crate) struct Second {
    place: RwLock<Option<Place>>,
    /// The folder of this run in the place (`lives/<life>/`).
    pub(crate) life: String,
    state: Mutex<CopyState>,
    /// What was saved could not be read (another version wrote it).
    pub(crate) unreadable: Option<String>,
}

impl Second {
    /// The second copy saved in `engine`'s metadata, if any; this run's folder.
    pub(crate) fn open(engine: &Engine, started_ms: i64) -> Second {
        let life = life_name(started_ms, &super::random_token());
        let saved = engine.meta_entries(SAVED).map(|entries| entries.into_iter().find(|(key, _)| key == SAVED));
        let (place, unreadable) = match saved {
            Ok(None) => (None, None),
            Ok(Some((_, bytes))) => {
                match serde_json::from_slice::<Json>(&bytes).ok().map(|j| Place::from_json(&j, None)) {
                    Some(Ok(place)) => (Some(place), None),
                    _ => (None, Some("the second copy saved here cannot be read by this version".to_owned())),
                }
            }
            Err(e) => (None, Some(e.to_string())),
        };
        Second { place: RwLock::new(place), life, state: Mutex::new(CopyState::default()), unreadable }
    }

    /// Where the copies go, if anywhere.
    pub(crate) fn place(&self) -> Option<Place> {
        self.place.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn state(&self) -> CopyState {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Saves `place` (`None`: no second copy) in `engine`'s metadata, then
    /// puts it in force: the next copy starts at once.
    pub(crate) fn save(&self, engine: &Engine, place: Option<Place>) -> Result<()> {
        let bytes = place.as_ref().map(|p| p.to_json().to_string().into_bytes());
        engine.set_meta(SAVED, bytes.as_deref())?;
        *self.place.write().unwrap_or_else(|e| e.into_inner()) = place;
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = CopyState::default();
        Ok(())
    }

    /// A copy is due now (and none is being written): it is marked as running.
    pub(crate) fn take_turn(&self, now: i64) -> Option<Place> {
        let place = self.place()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.running || now < state.next_at {
            return None;
        }
        state.running = true;
        Some(place)
    }

    /// The copy that was running ended with `result`. Returns whether this
    /// changes how it goes (it starts failing, or works again), to be said
    /// once in the log.
    pub(crate) fn done(&self, now: i64, result: &Result<()>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.running = false;
        match result {
            Ok(()) => {
                let changed = state.failing_since.is_some();
                state.last_at = Some(now);
                state.last_error = None;
                state.failing_since = None;
                state.tries = 0;
                state.next_at = now + EVERY_MS;
                changed
            }
            Err(e) => {
                let changed = state.failing_since.is_none();
                state.last_error = Some(e.to_string());
                state.failing_since.get_or_insert(now);
                let wait = RETRY_FIRST_MS.saturating_mul(1 << state.tries.min(10)).min(RETRY_MOST_MS);
                state.tries += 1;
                state.next_at = now + wait;
                changed
            }
        }
    }
}

/// Writes a complete copy of each database (`(name, engine)`, the first
/// database named "") to this run's folder of `place`, the first database
/// last (its manifest says the run is complete), then removes the runs older
/// than the [`KEPT`] newest complete ones.
pub(crate) fn copy_all(place: &Place, life: &str, databases: &[(String, Arc<Engine>)]) -> Result<()> {
    let root = place.store().map_err(|r| Error::Validation(r.said(super::i18n::Lang::En)))?;
    let run = life_store(root.clone(), life);
    let mut ordered: Vec<&(String, Arc<Engine>)> = databases.iter().collect();
    ordered.sort_by_key(|(name, _)| name.is_empty());
    for (name, engine) in ordered {
        let store: Arc<dyn BackupStore> = if name.is_empty() {
            run.clone()
        } else {
            Arc::new(PrefixedStore::new(run.clone(), format!("apps/{name}/")))
        };
        engine.copy_to(store.as_ref())?;
    }
    prune(place, root.as_ref(), life)?;
    Ok(())
}

/// The lines that start a server from the run `life` of `place`: the
/// environment's destination named there (the secret, never written out).
pub(crate) fn restore_lines(place: &Place, life: &str) -> Vec<String> {
    match place {
        Place::Dir(path) => vec![format!("NUAGE_BACKUP_DIR={}", path.join(LIVES).join(life).display())],
        Place::S3 { endpoint, bucket, region, prefix, access_key_id, .. } => vec![
            format!("NUAGE_BACKUP_S3_ENDPOINT={endpoint}"),
            format!("NUAGE_BACKUP_S3_BUCKET={bucket}"),
            format!("NUAGE_BACKUP_S3_REGION={region}"),
            format!("NUAGE_BACKUP_S3_PREFIX={prefix}{LIVES}{life}/"),
            format!("NUAGE_BACKUP_S3_ACCESS_KEY_ID={access_key_id}"),
            "NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=…".to_owned(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nuage-second-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn places() {
        for text in refused::ALL {
            assert!(crate::server::i18n::translated(text), "no translation: {text}");
        }
        let dir = Place::from_json(&json!({ "kind": "dir", "path": "/mnt/backup/nuage" }), None).unwrap();
        assert_eq!(dir, Place::Dir("/mnt/backup/nuage".into()));
        assert_eq!(Place::from_json(&dir.to_json(), None).unwrap(), dir, "kept as it was said");
        assert!(Place::from_json(&json!({ "kind": "dir", "path": "backups" }), None).is_err(), "not from the root");
        assert!(Place::from_json(&json!({ "kind": "tape" }), None).is_err());

        let said = json!({ "kind": "s3", "bucket": "copies", "access_key_id": "id", "secret_access_key": "s3cr3t" });
        let s3 = Place::from_json(&said, None).unwrap();
        assert_eq!(s3.describe(), "s3://copies/nuage/ (https://s3.us-east-1.amazonaws.com)");
        // The secret is kept, and never shown: not by the API, not in a log.
        assert_eq!(s3.to_json()["secret_access_key"], "s3cr3t");
        assert!(!s3.shown().to_string().contains("s3cr3t") && !format!("{s3:?}").contains("s3cr3t"));
        assert_eq!(Place::from_json(&s3.to_json(), None).unwrap(), s3);
        // Saved again without its secret: the one it had, while it names the same storage.
        let again = json!({ "kind": "s3", "bucket": "copies", "access_key_id": "id", "prefix": "other" });
        let Place::S3 { secret_access_key, prefix, .. } = Place::from_json(&again, Some(&s3)).unwrap() else {
            panic!()
        };
        assert_eq!((secret_access_key.as_str(), prefix.as_str()), ("s3cr3t", "other/"));
        let elsewhere = json!({ "kind": "s3", "bucket": "another", "access_key_id": "id" });
        assert!(Place::from_json(&elsewhere, Some(&s3)).is_err(), "another bucket: its secret is asked");
        assert!(Place::from_json(&json!({ "kind": "s3", "bucket": "copies" }), None).is_err());
    }

    #[test]
    fn a_folder_is_checked_and_must_stay_there() {
        let root = temp("folder");
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        // Inside the data directory, or with a parent that is not there: refused.
        assert_eq!(Place::Dir(data.join("copies")).check_folder(&data).unwrap_err().text, refused::IN_DATA);
        assert_eq!(Place::Dir(root.join("absent/disk/copies")).check_folder(&data).unwrap_err().text, refused::PARENT);
        // Only the last folder is made.
        let place = Place::Dir(root.join("copies"));
        place.check_folder(&data).unwrap();
        assert!(root.join("copies").is_dir());
        let store = place.store().unwrap();
        probe(store.as_ref()).unwrap();
        assert!(store.list("nuage-probe").unwrap().is_empty(), "the probe leaves nothing");
        // Its disk unplugged: writing fails, and nothing takes its place.
        std::fs::remove_dir_all(root.join("copies")).unwrap();
        let refused = probe(store.as_ref()).unwrap_err();
        assert!(refused.said(crate::server::i18n::Lang::En).contains("is not there"), "{refused:?}");
        assert!(!root.join("copies").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn each_run_has_its_folder_and_the_last_ones_are_kept() {
        let root = temp("lives");
        let folder = Place::Dir(root.clone());
        let place = folder.store().unwrap();
        // Names sort as the starts do.
        let day = 86_400_000;
        let names: Vec<String> = (0..6).map(|i| life_name(1_790_000_000_000 + i * day, "abcdef123")).collect();
        assert_eq!(names[0], "2026-09-21T14-13-20Z-abcdef");
        assert!(names.windows(2).all(|w| w[0] < w[1]));
        // Five runs: the first application and one other; the fourth never finished.
        for (i, life) in names.iter().take(5).enumerate() {
            let run = life_store(place.clone(), life);
            run.put("snapshots/00000000000000000001.db", b"pages").unwrap();
            run.put("apps/shop/snapshots/00000000000000000001.db", b"pages").unwrap();
            run.put("apps/shop/manifest.json", b"{}").unwrap();
            if i != 3 {
                run.put("manifest.json", b"{}").unwrap();
            }
        }
        assert_eq!(lives(place.as_ref()).unwrap(), names[..5]);
        assert!(complete(place.as_ref(), &names[0]) && !complete(place.as_ref(), &names[3]));
        // The sixth run starts: the three newest complete ones stay (0 is older: it goes),
        // and an unfinished run newer than the oldest kept one is left alone.
        assert_eq!(prune(&folder, place.as_ref(), &names[5]).unwrap(), vec![names[0].clone()]);
        assert!(!root.join("lives").join(&names[0]).exists());
        for kept in [&names[1], &names[2], &names[3], &names[4]] {
            assert!(root.join("lives").join(kept).join("apps/shop/manifest.json").exists(), "{kept}");
        }
        // Nothing more to remove; and the run being written is never removed.
        assert!(prune(&folder, place.as_ref(), &names[5]).unwrap().is_empty());
        assert!(
            prune(&folder, place.as_ref(), &names[1]).unwrap().is_empty(),
            "there are not three complete ones besides it"
        );
        // A name that no run has is no folder to remove.
        assert!(folder.remove_life(place.as_ref(), "../data").is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
