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
        assert!(events.iter().any(|e| e.message.contains("was imported")), "{events:?}");
        db.sql("INSERT INTO notes (label, grp, body) VALUES ('written-by-this-version', 5, 'n')", &[]).unwrap();
        assert_eq!(db.engine().encryption().is_some(), encryption.is_some());
    }
    assert!(dir.join("nuage.db").exists() && dir.join("nuage.redb.imported").exists());
    assert!(!dir.join("nuage.redb").exists(), "the old file is set aside");
    // Again: nothing left to import, and the write is there.
    let db = open_when_free(&dir, options(encryption));
    let (events, _) = db.engine().events();
    assert!(!events.iter().any(|e| e.message.contains("was imported")), "{events:?}");
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

/// A new, empty disk restores a backup of the earlier version; the backup
/// then gets a snapshot of this version, which restores in turn.
fn restore_backup(name: &str, encryption: Option<Encryption>) {
    let backup = copy_of(name, "backup");
    let dir = std::env::temp_dir().join(format!("nuage-legacy-restored-{name}-{}", std::process::id()));
    let again = std::env::temp_dir().join(format!("nuage-legacy-restored-again-{name}-{}", std::process::id()));
    for d in [&dir, &again] {
        let _ = std::fs::remove_dir_all(d);
    }
    let with_backup = || EngineOptions {
        backup: Some(BackupOptions::new(Arc::new(DirectoryStore::new(&backup)))),
        ..options(encryption.clone())
    };
    let gone = {
        let db = Embedded::open_with(&dir, with_backup()).unwrap();
        assert!(db.engine().backup_status().restored);
        check(&db, false);
        let status = db.engine().backup_status();
        assert!(status.last_ok && status.last_snapshot_at.is_some(), "{status:?}");
        db.sql("INSERT INTO notes (label, grp, body) VALUES ('after-the-import', 6, 'n')", &[]).unwrap();
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
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
    let db = Embedded::open_with(&again, with_backup()).unwrap();
    assert!(db.engine().backup_status().restored);
    assert!(again.join("nuage.db").exists() && !again.join("nuage.redb.imported").exists());
    assert_eq!(one(&db, "SELECT count(*) FROM notes"), "353");
    assert_eq!(one(&db, "SELECT count(*) FROM notes WHERE label = 'after-the-import'"), "1");
    drop(db);
    for d in [&dir, &again, &backup] {
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
