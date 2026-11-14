//! Data written by Nuage DB 0.4, whose page store was a redb file: a data
//! directory, closed or left by a crash, and a backup open with this version
//! with everything they held.
#![cfg(feature = "import-redb")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memoire_orm::Value;
use nuage::{BackupOptions, DirectoryStore, Embedded, Encryption, EngineOptions};
use serde_json::Value as Json;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/redb-0.4").join(name)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            copy_dir(&path, &to.join(entry.file_name()));
        } else {
            std::fs::copy(&path, to.join(entry.file_name())).unwrap();
        }
    }
}

/// A copy of a fixture to work on.
fn copy_of(name: &str, test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-legacy-{test}-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&fixture(name), &dir);
    dir
}

fn key() -> Option<Encryption> {
    Some(Encryption::from_key([42; 32]))
}

/// The first value of the first row, as text.
fn one(db: &Embedded, sql: &str) -> String {
    text(&db.sql(sql, &[]).unwrap()[0].rows[0][0])
}

fn text(value: &Value) -> String {
    match value {
        Value::I32(n) => n.to_string(),
        Value::I64(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Text(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// What every fixture holds. `log`: the commits made after the last
/// checkpoint are there too (a data directory, not a backup).
fn check(db: &Embedded, log: bool) {
    assert_eq!(one(db, "SELECT count(*) FROM notes"), "352");
    assert_eq!(one(db, "SELECT count(*) FROM notes WHERE id BETWEEN 100 AND 149"), "0", "deleted rows");
    assert_eq!(one(db, "SELECT body FROM notes WHERE id = 7"), "changed");
    assert_eq!(one(db, "SELECT body FROM notes WHERE id = 20"), "main-after-branch");
    assert_eq!(one(db, "SELECT length(body) FROM notes WHERE label = 'big'"), "20000", "a row larger than a page");
    assert_eq!(one(db, "SELECT count(*) FROM notes WHERE grp = 3"), "51", "by an index");
    assert_eq!(one(db, "SELECT count(*) FROM notes WHERE grp = 4"), if log { "51" } else { "50" }, "by an index");
    assert_eq!(one(db, "SELECT count(*) FROM notes WHERE label = 'after-snapshot'"), "1");
    assert_eq!(one(db, "SELECT count(*) FROM notes WHERE label = 'only-in-log'"), i32::from(log).to_string());
    assert_eq!(one(db, "SELECT count(*) FROM notes WHERE id = 30"), i32::from(!log).to_string());
    assert!(db.sql("SELECT * FROM gone", &[]).is_err(), "the dropped table stays dropped");
    let labels = db.sql("SELECT label FROM notes ORDER BY id LIMIT 3", &[]).unwrap();
    let labels: Vec<String> = labels[0].rows.iter().map(|r| text(&r[0])).collect();
    assert_eq!(labels, ["label-0000", "label-0001", "label-0002"]);

    // The branch: its own deletions and row, and the main database as it was when it was made.
    assert_eq!(db.engine().branches().iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), vec!["preview"]);
    let preview = db.on_branch("preview").unwrap();
    assert_eq!(one(&preview, "SELECT count(*) FROM notes"), "342");
    assert_eq!(one(&preview, "SELECT count(*) FROM notes WHERE id <= 10"), "0");
    assert_eq!(one(&preview, "SELECT count(*) FROM notes WHERE label = 'only-in-preview'"), "1");
    assert_eq!(one(&preview, "SELECT body LIKE 'body-19-%' FROM notes WHERE id = 20"), "true");
    assert_eq!(one(&preview, "SELECT count(*) FROM notes WHERE label = 'after-snapshot'"), "0");

    // Keys carry on, unique values stay checked, and the store is sound.
    let err = db.sql("INSERT INTO notes (label, grp, body) VALUES ('label-0001', 1, 'x')", &[]).unwrap_err();
    assert!(err.is_unique_violation(), "{err}");
    assert!(db.engine().check_integrity().unwrap());
}

fn options(encryption: Option<Encryption>) -> EngineOptions {
    EngineOptions { encryption, ..EngineOptions::default() }
}

/// Opens a copy of the data directory `name`, checks it, writes to it and opens it again.
fn open_directory(name: &str, encryption: Option<Encryption>) {
    let dir = copy_of(name, "dir");
    {
        let db = Embedded::open_with(&dir, options(encryption.clone())).unwrap();
        check(&db, true);
        let (events, _) = db.engine().events();
        assert!(events.iter().any(|e| e.message.contains("was imported and compared")), "{events:?}");
        let imported = db.engine().imported().expect("imported");
        assert!(imported.entries > 352 && imported.bytes > 20_000, "{imported:?}");
        db.sql("INSERT INTO notes (label, grp, body) VALUES ('written-by-this-version', 5, 'n')", &[]).unwrap();
        assert_eq!(db.engine().encryption().is_some(), encryption.is_some());
    }
    assert!(dir.join("nuage.db").exists() && dir.join("nuage.redb.imported").exists());
    assert!(!dir.join("nuage.redb").exists(), "the old file is set aside");
    // Again: nothing left to import, and the write is there.
    let db = open_when_free(&dir, options(encryption));
    let (events, _) = db.engine().events();
    assert!(!events.iter().any(|e| e.message.contains("was imported")), "{events:?}");
    assert_eq!(db.engine().imported(), None);
    assert_eq!(one(&db, "SELECT count(*) FROM notes"), "353");
    assert_eq!(one(&db, "SELECT max(id) FROM notes"), "404");
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Opens `dir` once the handle dropped just before has let go of it.
fn open_when_free(dir: &Path, options: EngineOptions) -> Embedded {
    for _ in 0..400 {
        match Embedded::open_with(dir, options.clone()) {
            Ok(db) => return db,
            Err(e) if e.to_string().contains("already open") => {
                std::thread::sleep(std::time::Duration::from_millis(25))
            }
            Err(e) => panic!("{e}"),
        }
    }
    panic!("{} stays open", dir.display())
}

#[test]
fn a_directory_closed_by_the_earlier_version() {
    open_directory("clear-closed", None);
}

#[test]
fn a_directory_left_by_a_crash_of_the_earlier_version() {
    open_directory("clear-crashed", None);
}

#[test]
fn an_encrypted_directory_left_by_a_crash() {
    open_directory("encrypted-crashed", key());
}

#[test]
fn an_encrypted_directory_needs_its_key() {
    let dir = copy_of("encrypted-crashed", "nokey");
    let err = Embedded::open(&dir).unwrap_err().to_string();
    assert!(err.contains("NUAGE_ENCRYPTION_KEY"), "{err}");
    let wrong = Embedded::open_with(&dir, options(Some(Encryption::from_key([1; 32])))).unwrap_err().to_string();
    assert!(wrong.contains("wrong encryption key"), "{wrong}");
    assert!(dir.join("nuage.redb").exists() && !dir.join("nuage.db").exists(), "nothing was touched");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_directory_in_clear_is_imported_then_encrypted() {
    let dir = copy_of("clear-crashed", "encrypt");
    {
        let db = Embedded::open_with(&dir, options(key())).unwrap();
        assert_eq!(db.engine().encryption(), Some("key"));
        check(&db, true);
    }
    // Nothing in clear is left: not the old file, not its copy.
    assert!(!dir.join("nuage.redb").exists() && !dir.join("nuage.redb.imported").exists());
    let raw = std::fs::read(dir.join("nuage.db")).unwrap();
    assert!(!raw.windows(10).any(|w| w == b"label-0001"));
    let db = open_when_free(&dir, options(key()));
    check(&db, true);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every file under `dir`, by its path from it.
fn files(dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let name = path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                out.insert(name, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    if dir.exists() {
        walk(dir, dir, &mut out);
    }
    out
}

/// A new, empty disk restores a backup of the earlier version. That backup is
/// set aside, whole, before this version writes its own over it, and stays
/// there however many snapshots follow: the database as the earlier version
/// left it can still be restored from it.
fn restore_backup(name: &str, encryption: Option<Encryption>) {
    let original = files(&fixture(name));
    let backup = copy_of(name, "backup");
    let tmp = |what: &str| std::env::temp_dir().join(format!("nuage-legacy-{what}-{name}-{}", std::process::id()));
    let (dir, again, back) = (tmp("restored"), tmp("restored-again"), tmp("rolled-back"));
    for d in [&dir, &again, &back] {
        let _ = std::fs::remove_dir_all(d);
    }
    let with = |store: &Path| EngineOptions {
        backup: Some(BackupOptions::new(Arc::new(DirectoryStore::new(store)))),
        ..options(encryption.clone())
    };
    let gone = {
        let db = Embedded::open_with(&dir, with(&backup)).unwrap();
        assert!(db.engine().backup_status().restored);
        check(&db, false);
        // Opening does not wait for it: the snapshot of this version follows in a moment.
        for _ in 0..400 {
            if db.engine().backup_status().last_snapshot_at.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let status = db.engine().backup_status();
        assert!(status.last_ok && status.last_snapshot_at.is_some(), "{status:?}");
        let (events, _) = db.engine().events();
        assert!(events.iter().any(|e| e.message.contains("before-import")), "{events:?}");
        // The earlier version's backup, byte for byte, where nothing prunes it.
        assert_eq!(files(&backup.join("before-import")), original);
        db.sql("INSERT INTO notes (label, grp, body) VALUES ('after-the-import', 6, 'n')", &[]).unwrap();
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
        // Snapshot after snapshot: the earlier version's own snapshots go, the copy aside stays.
        for n in 0..4 {
            db.sql("INSERT INTO notes (label, grp, body) VALUES (?, 6, 'n')", &[format!("later-{n}").into()]).unwrap();
            db.engine().backup_now(true).unwrap();
        }
        assert!(!files(&backup).keys().any(|k| k.starts_with("snapshots/") && k.ends_with(".redb")));
        assert_eq!(files(&backup.join("before-import")), original);
        Arc::downgrade(db.engine())
    };
    for _ in 0..400 {
        if gone.upgrade().is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let manifest: Json = serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    let snapshot = manifest["snapshot"].as_str().unwrap();
    assert!(snapshot.ends_with(".db"), "the latest snapshot is of this version: {snapshot}");
    {
        // Another empty disk: restored from the snapshot of this version, nothing to import.
        let db = Embedded::open_with(&again, with(&backup)).unwrap();
        assert!(db.engine().backup_status().restored);
        assert!(again.join("nuage.db").exists() && !again.join("nuage.redb.imported").exists());
        assert_eq!(one(&db, "SELECT count(*) FROM notes"), "357");
        assert_eq!(one(&db, "SELECT count(*) FROM notes WHERE label = 'after-the-import'"), "1");
    }
    {
        // Going back: the copy set aside is a whole backup of the earlier version.
        let db = Embedded::open_with(&back, with(&backup.join("before-import"))).unwrap();
        assert!(db.engine().backup_status().restored);
        check(&db, false);
        assert_eq!(one(&db, "SELECT count(*) FROM notes WHERE label = 'after-the-import'"), "0");
    }
    // Wait for the handles above to let go before the directories are removed.
    std::thread::sleep(std::time::Duration::from_millis(400));
    for d in [&dir, &again, &back, &backup] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn a_backup_of_the_earlier_version() {
    restore_backup("clear-backup", None);
}

#[test]
fn an_encrypted_backup_of_the_earlier_version() {
    restore_backup("encrypted-backup", key());
}

/// The backup of a server's application (under `apps/<name>/` of the backup
/// location) is set aside where a server whose backup location is
/// `before-import` looks for it: going back finds every application.
#[test]
fn the_backup_of_an_application_is_set_aside_where_a_server_looks_for_it() {
    use nuage::{BackupStore, PrefixedStore};
    let tmp = |what: &str| std::env::temp_dir().join(format!("nuage-legacy-app-{what}-{}", std::process::id()));
    let (root, dir, back) = (tmp("location"), tmp("data"), tmp("back"));
    for d in [&root, &dir, &back] {
        let _ = std::fs::remove_dir_all(d);
    }
    let original = files(&fixture("clear-backup"));
    copy_dir(&fixture("clear-backup"), &root.join("apps/shop"));
    // What a server gives the engine of its application `shop`.
    let of_shop = |location: &Path| -> EngineOptions {
        let store: Arc<dyn BackupStore> =
            Arc::new(PrefixedStore::new(Arc::new(DirectoryStore::new(location)), "apps/shop/"));
        EngineOptions { backup: Some(BackupOptions::new(store)), ..options(None) }
    };
    {
        let db = Embedded::open_with(&dir, of_shop(&root)).unwrap();
        assert!(db.engine().backup_status().restored);
        check(&db, false);
        db.engine().backup_now(true).unwrap();
        let (events, _) = db.engine().events();
        let told = events.iter().any(|e| e.message.contains("before-import/apps/shop"));
        assert!(told, "the event says where: {events:?}");
    }
    assert_eq!(files(&root.join("before-import/apps/shop")), original, "byte for byte, at the application's place");
    assert!(!root.join("apps/shop/before-import").exists());
    {
        // The same server, its backup location now `before-import`.
        let db = Embedded::open_with(&back, of_shop(&root.join("before-import"))).unwrap();
        assert!(db.engine().backup_status().restored, "the application is found there");
        check(&db, false);
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    for d in [&root, &dir, &back] {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// An idle database of the earlier version, and one snapshot kept: the
/// first snapshot of this version, at the same place in the log as the old
/// one, is the one that stays.
#[test]
fn the_first_snapshot_after_an_import_is_the_one_kept() {
    let tmp = |what: &str| std::env::temp_dir().join(format!("nuage-legacy-idle-{what}-{}", std::process::id()));
    let (backup, dir, again) = (tmp("backup"), tmp("data"), tmp("again"));
    for d in [&backup, &dir, &again] {
        let _ = std::fs::remove_dir_all(d);
    }
    copy_dir(&fixture("clear-backup"), &backup);
    // Nothing was written after its snapshot.
    std::fs::remove_dir_all(backup.join("wal")).unwrap();
    let with = || {
        let store = Arc::new(DirectoryStore::new(&backup));
        EngineOptions {
            backup: Some(BackupOptions { keep_snapshots: 1, ..BackupOptions::new(store) }),
            ..options(None)
        }
    };
    let rows = {
        let db = Embedded::open_with(&dir, with()).unwrap();
        db.engine().backup_now(true).unwrap();
        one(&db, "SELECT count(*) FROM notes")
    };
    let manifest: Json = serde_json::from_slice(&std::fs::read(backup.join("manifest.json")).unwrap()).unwrap();
    let snapshot = manifest["snapshot"].as_str().unwrap();
    assert_eq!(snapshot, "snapshots/00000000000000000410.db", "at the place of the old one in the log");
    let left: Vec<String> = files(&backup).into_keys().filter(|k| k.starts_with("snapshots/")).collect();
    assert_eq!(left, [snapshot], "the old snapshot went, not the one the manifest names");
    {
        let db = Embedded::open_with(&again, with()).unwrap();
        assert!(db.engine().backup_status().restored);
        assert_eq!(one(&db, "SELECT count(*) FROM notes"), rows);
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    for d in [&backup, &dir, &again] {
        let _ = std::fs::remove_dir_all(d);
    }
}
