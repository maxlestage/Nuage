//! Encryption at rest: nothing Nuage DB writes (pages, log, backups) holds
//! the data in clear, a wrong or missing key is refused, and a database or a
//! backup stored in clear is encrypted when it is opened with a key.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use memoire_orm::prelude::*;
use nuage::{BackupOptions, BackupStore, DirectoryStore, Embedded, Encryption, EngineOptions, MirrorStore};

const NEEDLE: &str = "TOP-SECRET-PAYLOAD";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-enc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "secrets")]
struct Secret {
    #[memoire(primary_key, auto_increment)]
    id: i64,
    /// Indexed: index keys hold the value too.
    #[memoire(unique)]
    label: String,
    body: String,
}

fn secret(n: i64) -> Secret {
    Secret { id: 0, label: format!("{NEEDLE}-label-{n}"), body: format!("{NEEDLE}-body-{n}") }
}

fn key() -> Encryption {
    Encryption::from_key([42; 32])
}

fn encrypted(extra: EngineOptions) -> EngineOptions {
    EngineOptions { encryption: Some(key()), ..extra }
}

/// Every file under `dir` that contains the needle.
fn files_with_needle(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_with_needle(&path));
        } else if std::fs::read(&path).unwrap().windows(NEEDLE.len()).any(|w| w == NEEDLE.as_bytes()) {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn nothing_in_clear_on_disk() {
    let dir = temp_dir("disk");
    {
        let db = Embedded::open_with(&dir, encrypted(EngineOptions::default())).unwrap();
        assert_eq!(db.engine().encryption(), Some("key"));
        db.create_table::<Secret>().await.unwrap();
        for n in 0..40 {
            secret(n).insert(&db).await.unwrap();
        }
        // Some rows in the page store, some only in the log.
        db.engine().checkpoint().unwrap();
        for n in 40..60 {
            secret(n).insert(&db).await.unwrap();
        }
        db.engine().create_branch("preview", None).unwrap();
        let wal: Vec<_> = std::fs::read_dir(dir.join("wal")).unwrap().flatten().collect();
        assert!(!wal.is_empty());
        assert_eq!(files_with_needle(&dir), Vec::<PathBuf>::new(), "while open");
    }
    assert_eq!(files_with_needle(&dir), Vec::<PathBuf>::new(), "after closing");
    assert!(dir.join("encryption.json").exists());

    // The right key reads everything back.
    let db = Embedded::open_with(&dir, encrypted(EngineOptions::default())).unwrap();
    assert_eq!(Secret::count(&db).await.unwrap(), 60);
    let found = Secret::query().filter(Secret::LABEL.eq(format!("{NEEDLE}-label-55"))).one(&db).await.unwrap();
    assert_eq!(found.body, format!("{NEEDLE}-body-55"));
    assert!(db.engine().check_integrity().unwrap());
    assert_eq!(db.engine().branches().len(), 1);
    drop(db);

    // No key, or another one: refused before anything is read.
    let err = Embedded::open(&dir).unwrap_err().to_string();
    assert!(err.contains("encrypted") && err.contains("NUAGE_ENCRYPTION_KEY"), "{err}");
    let wrong = EngineOptions { encryption: Some(Encryption::from_key([1; 32])), ..EngineOptions::default() };
    let err = Embedded::open_with(&dir, wrong).unwrap_err().to_string();
    assert!(err.contains("wrong encryption key"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_passphrase_works_too() {
    let dir = temp_dir("phrase");
    let phrase = || EngineOptions {
        encryption: Some(Encryption::parse("une longue phrase de passe").unwrap()),
        ..EngineOptions::default()
    };
    {
        let db = Embedded::open_with(&dir, phrase()).unwrap();
        assert_eq!(db.engine().encryption(), Some("passphrase"));
        db.create_table::<Secret>().await.unwrap();
        secret(1).insert(&db).await.unwrap();
    }
    assert!(files_with_needle(&dir).is_empty());
    let db = Embedded::open_with(&dir, phrase()).unwrap();
    assert_eq!(Secret::count(&db).await.unwrap(), 1);
    drop(db);
    let err = Embedded::open_with(&dir, encrypted(EngineOptions::default())).unwrap_err().to_string();
    assert!(err.contains("passphrase"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn data_and_backups_in_clear_are_encrypted_when_opened_with_a_key() {
    in_clear_then_encrypted(false).await;
}

/// The same with two backup destinations: each one's backup in clear is
/// replaced, and each one alone restores the encrypted database.
#[tokio::test]
async fn backups_in_clear_in_two_places_are_encrypted_too() {
    in_clear_then_encrypted(true).await;
}

async fn in_clear_then_encrypted(two: bool) {
    let end = if two { "-two" } else { "" };
    let dir = temp_dir(&format!("migrate{end}"));
    let target = temp_dir(&format!("migrate-backup{end}"));
    let second = temp_dir(&format!("migrate-second{end}"));
    let copy = temp_dir(&format!("migrate-copy{end}"));
    let folder = |at: &Path| -> Arc<dyn BackupStore> { Arc::new(DirectoryStore::new(at)) };
    let backup = || {
        let store = if two { Arc::new(MirrorStore::new(folder(&target), folder(&second))) } else { folder(&target) };
        BackupOptions::new(store)
    };
    let folders = if two { vec![target.clone(), second.clone()] } else { vec![target.clone()] };
    // A database of an earlier version: in clear, backed up in clear.
    let gone = {
        let db =
            Embedded::open_with(&dir, EngineOptions { backup: Some(backup()), ..EngineOptions::default() }).unwrap();
        assert_eq!(db.engine().encryption(), None);
        db.create_table::<Secret>().await.unwrap();
        for n in 0..25 {
            secret(n).insert(&db).await.unwrap();
        }
        db.engine().create_branch("review", None).unwrap();
        db.engine().backup_now(true).unwrap();
        for n in 25..30 {
            secret(n).insert(&db).await.unwrap();
        }
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
        Arc::downgrade(db.engine())
    };
    // Its upkeep thread lets go of it a moment later: wait, so that it does not
    // ship a backup while the next opening replaces them.
    for _ in 0..400 {
        if gone.upgrade().is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(gone.upgrade().is_none(), "the engine in clear is still open");
    assert!(!files_with_needle(&dir).is_empty(), "in clear before");
    for at in &folders {
        assert!(!files_with_needle(at).is_empty(), "in clear before: {}", at.display());
    }

    // Opened with a key: encrypted in place, and the backup replaced.
    {
        let db = Embedded::open_with(&dir, encrypted(EngineOptions { backup: Some(backup()), ..Default::default() }))
            .unwrap();
        assert_eq!(db.engine().encryption(), Some("key"));
        assert_eq!(Secret::count(&db).await.unwrap(), 30);
        assert_eq!(db.engine().branches()[0].name, "review");
        let (events, _) = db.engine().events();
        assert!(events.iter().any(|e| e.message.contains("now encrypted")), "{events:?}");
        let replaced = events.iter().filter(|e| e.message.contains("was replaced by an encrypted one")).count();
        assert_eq!(replaced, folders.len(), "{events:?}");
        secret(30).insert(&db).await.unwrap();
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
    }
    assert_eq!(files_with_needle(&dir), Vec::<PathBuf>::new());
    for at in &folders {
        assert_eq!(files_with_needle(at), Vec::<PathBuf>::new(), "the backup in clear is gone: {}", at.display());
        assert!(at.join("encryption.json").exists());
    }

    // A new, empty disk restores the encrypted backup, with the key only.
    let err = Embedded::open_with(&copy, EngineOptions { backup: Some(backup()), ..EngineOptions::default() })
        .unwrap_err()
        .to_string();
    assert!(err.contains("NUAGE_ENCRYPTION_KEY"), "{err}");
    let _ = std::fs::remove_dir_all(&copy);
    // From each place alone too, with two.
    for at in &folders {
        let store = if two { folder(at) } else { backup().store };
        let db = Embedded::open_with(
            &copy,
            encrypted(EngineOptions { backup: Some(BackupOptions::new(store)), ..Default::default() }),
        )
        .unwrap();
        assert!(db.engine().backup_status().restored);
        assert_eq!(Secret::count(&db).await.unwrap(), 31, "{}", at.display());
        let gone = Arc::downgrade(db.engine());
        drop(db);
        for _ in 0..400 {
            if gone.upgrade().is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(&copy);
    }
    for d in [dir, target, second, copy] {
        let _ = std::fs::remove_dir_all(&d);
    }
}
