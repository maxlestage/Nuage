//! The engine: durability, crash recovery, data larger than the cache,
//! branches, backups.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use memoire_orm::prelude::*;
use memoire_orm::{Query, Value};
use nuage::{BackupOptions, DirectoryStore, Embedded, EngineOptions};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "items")]
struct Item {
    #[memoire(primary_key, auto_increment)]
    id: i64,
    #[memoire(unique)]
    code: String,
    #[memoire(index)]
    group_no: i32,
    payload: String,
    score: f64,
}

fn item(n: i64) -> Item {
    Item {
        id: 0,
        code: format!("code-{n:08}"),
        group_no: (n % 100) as i32,
        payload: format!("{n:08}-{}", "x".repeat(180)),
        score: (n % 1000) as f64 / 10.0,
    }
}

#[tokio::test]
async fn data_survives_a_restart() {
    let dir = temp_dir("restart");
    {
        let db = Embedded::open(&dir).unwrap();
        db.create_table::<Item>().await.unwrap();
        for n in 0..50 {
            item(n).insert(&db).await.unwrap();
        }
        Item::delete_many().filter(Item::GROUP_NO.eq(7)).execute(&db).await.unwrap();
    }
    let db = Embedded::open(&dir).unwrap();
    assert_eq!(Item::count(&db).await.unwrap(), 49);
    assert_eq!(Item::find(&db, 8).await.unwrap(), None, "row 8 was in group 7");
    // Keys continue after the last one, and unique values stay checked.
    let mut it = item(1000);
    it.insert(&db).await.unwrap();
    assert_eq!(it.id, 51);
    assert!(item(3).insert(&db).await.unwrap_err().is_unique_violation());
    assert!(db.engine().check_integrity().unwrap());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The child half of `crash_recovery`: writes forever, printing each
/// committed key, until it is killed.
fn crash_child(dir: &str) -> ! {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let db = open_when_free(
            dir.as_ref(),
            EngineOptions {
                checkpoint_every: Duration::from_millis(50),
                encryption: crash_key(),
                ..EngineOptions::default()
            },
        );
        db.create_table_if_not_exists::<Item>().await.unwrap();
        // Rows of earlier rounds are kept: carry on after them.
        let mut n = Item::count(&db).await.unwrap() as i64;
        loop {
            // Single rows and batches, so the kill can land anywhere.
            let mut it = item(n);
            it.insert(&db).await.unwrap();
            let batch: Vec<Item> = (1..=5).map(|k| item(n + k)).collect();
            Item::insert_many(&db, &batch).await.unwrap();
            println!("{}", n + 5);
            n += 6;
        }
    })
}

/// Opens `dir`, waiting while another handle lets go of it: a dropped
/// database is closed by its upkeep thread when that thread is in the middle
/// of a checkpoint, a moment later.
fn open_when_free(dir: &std::path::Path, options: EngineOptions) -> Embedded {
    for _ in 0..200 {
        match Embedded::open_with(dir, options.clone()) {
            Ok(db) => return db,
            Err(e) if e.to_string().contains("already open") => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => panic!("{e}"),
        }
    }
    panic!("{} stays open", dir.display())
}

/// The key of the child, from `NUAGE_CRASH_KEY` (set: an encrypted database).
fn crash_key() -> Option<nuage::Encryption> {
    std::env::var("NUAGE_CRASH_KEY").ok().map(|k| nuage::Encryption::parse(&k).unwrap())
}

#[test]
fn crash_recovery() {
    crash_rounds("crash_recovery", None);
}

/// The same, with an encrypted page store and log.
#[test]
fn crash_recovery_encrypted() {
    crash_rounds("crash_recovery_encrypted", Some("5a".repeat(32)));
}

fn crash_rounds(test: &str, key: Option<String>) {
    if let Ok(dir) = std::env::var("NUAGE_CRASH_CHILD") {
        crash_child(&dir);
    }
    let dir = temp_dir(test);
    let options = || EngineOptions {
        encryption: key.as_deref().map(|k| nuage::Encryption::parse(k).unwrap()),
        ..EngineOptions::default()
    };
    for round in 0..3 {
        let mut command = Command::new(std::env::current_exe().unwrap());
        if let Some(k) = &key {
            command.env("NUAGE_CRASH_KEY", k);
        }
        let mut child = command
            .args(["--exact", test, "--nocapture", "--test-threads=1"])
            .env("NUAGE_CRASH_CHILD", &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut acknowledged = 0usize;
        let mut seen = 0;
        while let Some(Ok(line)) = lines.next() {
            if let Ok(n) = line.trim().parse::<usize>() {
                acknowledged = n;
                seen += 1;
                if seen >= 150 + round * 50 {
                    break;
                }
            }
        }
        // SIGKILL: no destructor, no flush, in the middle of whatever it does.
        child.kill().unwrap();
        child.wait().unwrap();

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let db = open_when_free(&dir, options());
            let count = Item::count(&db).await.unwrap() as usize;
            // `acknowledged` is the last row of the last group printed: every row up to it is
            // there (`+ 1`), then the commits in flight, whole or not at all: nothing more
            // (`+ 1`), the single row (`+ 2`), or the single row and the batch of five (`+ 7`).
            // Anything else is an acknowledged commit lost or a batch torn.
            assert!(
                matches!(count.checked_sub(acknowledged), Some(1 | 2 | 7)),
                "round {round}: {count} rows, {acknowledged} acknowledged: a commit was lost or torn"
            );
            let rows = Item::query().order_by(Item::ID.asc()).all(&db).await.unwrap();
            assert_eq!(rows.len(), count, "the row counter matches the rows");
            assert!(db.engine().check_integrity().unwrap());
        });
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A data directory of Nuage DB 0.4 (a redb page store) needs the `import-redb` feature: without
/// it, it is refused with a message that says so, and left as it is.
#[cfg(not(feature = "import-redb"))]
#[test]
fn an_earlier_page_store_is_refused_without_the_import_feature() {
    let dir = temp_dir("legacy-refused");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("nuage.redb"), b"redb").unwrap();
    let err = Embedded::open(&dir).unwrap_err().to_string();
    assert!(err.contains("import-redb"), "{err}");
    assert!(dir.join("nuage.redb").exists() && !dir.join("nuage.db").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn more_data_than_cache() {
    let dir = temp_dir("big");
    let cache = 1 << 20;
    let db = Embedded::open_with(&dir, EngineOptions { cache_bytes: cache, sync: false, ..EngineOptions::default() })
        .unwrap();
    db.create_table::<Item>().await.unwrap();
    let total = 40_000i64;
    for chunk in (0..total).collect::<Vec<_>>().chunks(2_000) {
        let rows: Vec<Item> = chunk.iter().map(|&n| item(n)).collect();
        Item::insert_many(&db, &rows).await.unwrap();
    }
    db.engine().checkpoint().unwrap();
    let stats = db.engine().stats();
    assert!(stats.file_bytes > 8 * cache as u64, "{} bytes on disk", stats.file_bytes);
    assert!(stats.cache_bytes <= 2 * cache as u64, "cache holds {} bytes", stats.cache_bytes);

    // Lookups read only what they need.
    assert_eq!(Item::count(&db).await.unwrap(), total as u64);
    assert_eq!(Item::get(&db, 31_337).await.unwrap().code, "code-00031336");
    let by_code = Item::query().filter(Item::CODE.eq("code-00012345")).one(&db).await.unwrap();
    assert_eq!(by_code.id, 12_346);
    assert_eq!(Item::query().filter(Item::GROUP_NO.eq(42)).count(&db).await.unwrap(), 400);
    let first = Item::query().filter(Item::SCORE.gt(99.0)).limit(3).all(&db).await.unwrap();
    assert_eq!(first.iter().map(|i| i.id).collect::<Vec<_>>(), vec![992, 993, 994]);
    let top = Item::query().order_by(Item::SCORE.desc()).order_by(Item::ID.asc()).limit(2).all(&db).await.unwrap();
    assert_eq!(top.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1000, 2000]);
    let page = Item::query().order_by(Item::ID.desc()).limit(5).offset(10).all(&db).await.unwrap();
    assert_eq!(page[0].id, total - 10);
    let stats = db.engine().stats();
    assert!(stats.cache_bytes <= 2 * cache as u64, "cache holds {} bytes", stats.cache_bytes);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn branches_are_isolated_copies() {
    let dir = temp_dir("branches");
    let db = Embedded::open(&dir).unwrap();
    db.create_table::<Item>().await.unwrap();
    for n in 0..10 {
        item(n).insert(&db).await.unwrap();
    }
    let engine = db.engine().clone();
    let b = engine.create_branch("preview-1", None).unwrap();
    assert_eq!((b.name.as_str(), b.parent.as_deref()), ("preview-1", None));
    let preview = db.on_branch("preview-1").unwrap();

    // The branch changes on its own...
    item(100).insert(&preview).await.unwrap();
    Item::delete_many().filter(Item::ID.le(3)).execute(&preview).await.unwrap();
    Item::update_many().set(Item::SCORE, 1.5).filter(Item::ID.eq(5)).execute(&preview).await.unwrap();
    assert_eq!(Item::count(&preview).await.unwrap(), 8);
    assert_eq!(Item::count(&db).await.unwrap(), 10, "main is untouched");
    // ...and keeps the main database as it was when it was made.
    Item::delete_many().filter(Item::ID.ge(9)).execute(&db).await.unwrap();
    let mut late = item(500);
    late.insert(&db).await.unwrap();
    Item::update_many().set(Item::PAYLOAD, "changed".to_owned()).filter(Item::ID.eq(6)).execute(&db).await.unwrap();
    assert_eq!(Item::count(&db).await.unwrap(), 9);
    assert_eq!(Item::count(&preview).await.unwrap(), 8);
    assert!(Item::find(&preview, 10).await.unwrap().is_some(), "deleted in main after the branch was made");
    let late_in_preview = Item::query().filter(Item::CODE.eq(late.code.clone())).count(&preview).await.unwrap();
    assert_eq!(late_in_preview, 0, "inserted in main after the branch was made");
    assert_ne!(Item::get(&preview, 6).await.unwrap().payload, "changed");
    assert_eq!(Item::get(&preview, 5).await.unwrap().score, 1.5);
    // Indexes and unique checks see the branch's own data.
    assert_eq!(Item::query().filter(Item::CODE.eq("code-00000100")).count(&preview).await.unwrap(), 1);
    assert_eq!(Item::query().filter(Item::CODE.eq("code-00000100")).count(&db).await.unwrap(), 0);
    assert!(item(100).insert(&preview).await.unwrap_err().is_unique_violation());
    item(100).insert(&db).await.unwrap();

    // A branch of the branch; deleting a parent with children is refused.
    engine.create_branch("preview-2", Some("preview-1")).unwrap();
    let child = db.on_branch("preview-2").unwrap();
    assert_eq!(Item::count(&child).await.unwrap(), 8);
    item(200).insert(&preview).await.unwrap();
    assert_eq!(Item::count(&child).await.unwrap(), 8, "the child keeps its parent's past");
    assert!(engine.delete_branch("preview-1").is_err());
    assert!(engine.create_branch("preview-1", None).is_err(), "names are unique");
    assert!(engine.create_branch("Bad Name", None).is_err());

    // Tables made in a branch stay there.
    #[derive(Model, Debug)]
    #[memoire(table = "only_in_branch")]
    struct Extra {
        #[memoire(primary_key)]
        id: i64,
    }
    child.create_table::<Extra>().await.unwrap();
    assert!(db.create_table::<Extra>().await.is_ok(), "main has no such table yet");

    // Reset: the branch forgets its changes and sees its parent again.
    assert!(engine.reset_branch("preview-2").unwrap());
    assert_eq!(Item::count(&child).await.unwrap(), Item::count(&preview).await.unwrap());
    assert_eq!(engine.branches().len(), 2);
    assert!(engine.delete_branch("preview-2").unwrap());
    assert!(engine.delete_branch("preview-1").unwrap());
    assert!(!engine.delete_branch("preview-1").unwrap());
    assert!(engine.branches().is_empty());
    assert!(db.on_branch("preview-1").is_err());
    assert_eq!(Item::count(&db).await.unwrap(), 10);

    // Branches survive a restart.
    engine.create_branch("kept", None).unwrap();
    drop((db, preview, child, engine));
    let db = Embedded::open(&dir).unwrap();
    assert_eq!(db.engine().branches().iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), vec!["kept"]);
    assert_eq!(Item::count(&db.on_branch("kept").unwrap()).await.unwrap(), 10);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn transactions_conflict_instead_of_losing_writes() {
    let dir = temp_dir("conflict");
    let db = Embedded::open(&dir).unwrap();
    db.create_table::<Item>().await.unwrap();
    item(1).insert(&db).await.unwrap();
    let a = db.begin().await.unwrap();
    let b = db.begin().await.unwrap();
    let mut x = Item::get(&a, 1).await.unwrap();
    let mut y = Item::get(&b, 1).await.unwrap();
    x.score = 10.0;
    y.score = 20.0;
    x.update(&a).await.unwrap();
    y.update(&b).await.unwrap();
    a.commit().await.unwrap();
    assert!(b.commit().await.unwrap_err().is_conflict());
    assert_eq!(Item::get(&db, 1).await.unwrap().score, 10.0);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A transaction goes by the data as it was when it began: a commit that
/// lands before the transaction first reads the table is a conflict too.
#[tokio::test]
async fn a_commit_before_the_first_read_is_a_conflict() {
    let dir = temp_dir("conflict-early");
    let db = Embedded::open(&dir).unwrap();
    db.create_table::<Item>().await.unwrap();
    item(1).insert(&db).await.unwrap();
    let tx = db.begin().await.unwrap();
    Item::update_many().set(Item::SCORE, 50.0).filter(Item::ID.eq(1)).execute(&db).await.unwrap();
    // The transaction still sees the row as it was, and must not write over the change.
    let mut seen = Item::get(&tx, 1).await.unwrap();
    assert_eq!(seen.score, 0.1);
    seen.score += 1.0;
    seen.update(&tx).await.unwrap();
    assert!(tx.commit().await.unwrap_err().is_conflict());
    assert_eq!(Item::get(&db, 1).await.unwrap().score, 50.0);
    // A transaction that began after it commits.
    let tx = db.begin().await.unwrap();
    let mut seen = Item::get(&tx, 1).await.unwrap();
    seen.score += 1.0;
    seen.update(&tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(Item::get(&db, 1).await.unwrap().score, 51.0);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Writers at the same time, the log synced (several commits share a sync):
/// every commit that returned is read at once by its writer, and is there
/// after a restart.
#[test]
fn writers_at_the_same_time_share_the_log() {
    let dir = temp_dir("together");
    let (threads, each) = (8i64, 40i64);
    {
        let db = Embedded::open(&dir).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(db.create_table::<Item>()).unwrap();
        std::thread::scope(|scope| {
            for t in 0..threads {
                let db = db.clone();
                scope.spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                    rt.block_on(async {
                        for n in 0..each {
                            let mut it = item(t * 1_000_000 + n);
                            it.insert(&db).await.unwrap();
                            // Its own commit, read back at once (a plain read sees what is on disk).
                            let back = Item::find(&db, it.id).await.unwrap().expect("a commit that returned is read");
                            assert_eq!(back.code, it.code);
                            if n % 10 == 0 {
                                // A transaction among them: retried when another commit got there first.
                                loop {
                                    let tx = db.begin().await.unwrap();
                                    let mut row = Item::get(&tx, it.id).await.unwrap();
                                    row.score += 1.0;
                                    row.update(&tx).await.unwrap();
                                    match tx.commit().await {
                                        Ok(()) => break,
                                        Err(e) if e.is_conflict() => continue,
                                        Err(e) => panic!("{e}"),
                                    }
                                }
                            }
                        }
                    });
                });
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(Item::count(&db)).unwrap(), (threads * each) as u64);
    }
    let db = open_when_free(&dir, EngineOptions::default());
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        let rows = Item::query().order_by(Item::ID.asc()).all(&db).await.unwrap();
        assert_eq!(rows.len() as i64, threads * each);
        let ids: std::collections::BTreeSet<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids.len(), rows.len(), "every row its own id");
        let codes: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.code.as_str()).collect();
        assert_eq!(codes.len(), rows.len());
        assert!(db.engine().check_integrity().unwrap());
    });
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Batches sent at the same time each take their turn: none is refused for a
/// conflict, each applies whole.
#[test]
fn batches_at_the_same_time_take_turns() {
    let dir = temp_dir("batches");
    let db = Embedded::open(&dir).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(db.create_table::<Item>()).unwrap();
    let (threads, each) = (8i64, 25i64);
    std::thread::scope(|scope| {
        for t in 0..threads {
            let db = &db;
            scope.spawn(move || {
                for n in 0..each {
                    let rows: Vec<Vec<Value>> = (0..2)
                        .map(|k| {
                            let it = item(t * 1_000_000 + n * 2 + k);
                            vec![
                                Value::Null,
                                Value::Text(it.code),
                                Value::I32(it.group_no),
                                Value::Text(it.payload),
                                Value::F64(it.score),
                            ]
                        })
                        .collect();
                    let statements: Vec<(bool, Query<'_>)> = rows
                        .iter()
                        .map(|values| (false, Query::InsertRow { table: Item::SCHEMA, values }))
                        .chain([(true, Query::Select(&COUNT))])
                        .collect();
                    let results = db.batch(&statements).unwrap();
                    assert_eq!(results.len(), 3);
                    // A batch of reads alone, in between.
                    assert_eq!(db.batch(&[(true, Query::Select(&COUNT))]).unwrap().len(), 1);
                }
            });
        }
    });
    assert_eq!(rt.block_on(Item::count(&db)).unwrap() as i64, threads * each * 2);
    // A batch that fails leaves nothing behind.
    let taken =
        vec![Value::Null, Value::Text("code-00000000".into()), Value::I32(1), Value::Text("p".into()), Value::F64(0.0)];
    let fresh = vec![Value::Null, Value::Text("fresh".into()), Value::I32(1), Value::Text("p".into()), Value::F64(0.0)];
    let failing = [
        (false, Query::InsertRow { table: Item::SCHEMA, values: &fresh }),
        (false, Query::InsertRow { table: Item::SCHEMA, values: &taken }),
    ];
    assert!(db.batch(&failing).unwrap_err().is_unique_violation());
    assert_eq!(rt.block_on(Item::count(&db)).unwrap() as i64, threads * each * 2);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `SELECT * FROM items`, for the batches above.
static COUNT: std::sync::LazyLock<memoire_orm::ast::SelectAst> =
    std::sync::LazyLock::new(|| memoire_orm::ast::SelectAst::new(Item::SCHEMA));

/// The child half of `crash_recovery_of_writers_at_the_same_time`: four
/// threads write forever, each printing the code of every row whose commit
/// returned, until the process is killed.
fn crash_child_threads(dir: &str) -> ! {
    let db = open_when_free(
        dir.as_ref(),
        EngineOptions { checkpoint_every: Duration::from_millis(50), ..EngineOptions::default() },
    );
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(db.create_table_if_not_exists::<Item>()).unwrap();
    let start = rt.block_on(Item::count(&db)).unwrap() as i64;
    std::thread::scope(|scope| {
        for t in 0..4i64 {
            let db = db.clone();
            scope.spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    for n in 0..i64::MAX {
                        // Codes of earlier rounds are not used again.
                        let it = item((start + 1) * 10_000_000 + t * 1_000_000 + n);
                        it.clone().insert(&db).await.unwrap();
                        println!("{}", it.code);
                    }
                });
            });
        }
    });
    unreachable!("the writers never stop")
}

/// Killed while four threads commit: every commit that returned is there.
#[test]
fn crash_recovery_of_writers_at_the_same_time() {
    if let Ok(dir) = std::env::var("NUAGE_CRASH_CHILD") {
        crash_child_threads(&dir);
    }
    let dir = temp_dir("crash-together");
    let mut acknowledged = std::collections::BTreeSet::new();
    for round in 0..3 {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_recovery_of_writers_at_the_same_time", "--nocapture", "--test-threads=1"])
            .env("NUAGE_CRASH_CHILD", &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut seen = 0;
        while let Some(Ok(line)) = lines.next() {
            if line.starts_with("code-") {
                acknowledged.insert(line.trim().to_owned());
                seen += 1;
                if seen >= 200 + round * 100 {
                    break;
                }
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let db = open_when_free(&dir, EngineOptions::default());
            let rows = Item::query().all(&db).await.unwrap();
            let stored: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.code.as_str()).collect();
            let lost: Vec<&String> = acknowledged.iter().filter(|c| !stored.contains(c.as_str())).collect();
            assert!(lost.is_empty(), "round {round}: commits that returned are gone: {lost:?}");
            assert_eq!(Item::count(&db).await.unwrap() as usize, rows.len(), "the row counter matches the rows");
            assert!(db.engine().check_integrity().unwrap());
        });
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn backup_and_restore_into_an_empty_directory() {
    let dir = temp_dir("backup-src");
    let target = temp_dir("backup-dst");
    let copy = temp_dir("backup-copy");
    let backup = || BackupOptions::new(Arc::new(DirectoryStore::new(&target)));
    {
        let db =
            Embedded::open_with(&dir, EngineOptions { backup: Some(backup()), ..EngineOptions::default() }).unwrap();
        db.create_table::<Item>().await.unwrap();
        for n in 0..20 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().backup_now(true).unwrap();
        // Written after the snapshot: they travel in log segments.
        for n in 20..30 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().create_branch("after-snapshot", None).unwrap();
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
        let status = db.engine().backup_status();
        assert!(status.last_ok && status.last_snapshot_at.is_some(), "{status:?}");
    }
    // A fresh, empty disk (a new container): the backup comes back.
    let db = Embedded::open_with(&copy, EngineOptions { backup: Some(backup()), ..EngineOptions::default() }).unwrap();
    assert!(db.engine().backup_status().restored);
    assert_eq!(Item::count(&db).await.unwrap(), 30);
    assert_eq!(Item::get(&db, 30).await.unwrap().code, "code-00000029");
    assert_eq!(db.engine().branches().len(), 1);
    drop(db);
    for d in [dir, target, copy] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// `WHERE indexed = ? ORDER BY id [DESC] LIMIT n [OFFSET m]` stops reading
/// early: it still gives exactly what reading everything gives.
#[tokio::test]
async fn early_stop_through_an_index_gives_the_same_rows() {
    let dir = temp_dir("early");
    let db = Embedded::open(&dir).unwrap();
    db.create_table::<Item>().await.unwrap();
    for n in 0..600 {
        item(n).insert(&db).await.unwrap();
    }
    // Some rows of the group deleted and changed, so the index has gaps.
    Item::delete_many().filter(Item::ID.between(100, 140)).execute(&db).await.unwrap();
    Item::update_many()
        .set(Item::SCORE, 99.0)
        .filter(Item::GROUP_NO.eq(7))
        .filter(Item::ID.gt(300))
        .execute(&db)
        .await
        .unwrap();
    for (desc, limit, offset, extra) in [
        (true, 5, 0, false),
        (false, 5, 0, false),
        (true, 3, 2, false),
        (true, 4, 0, true),
        (false, 2, 1, true),
        (true, 50, 0, false),
    ] {
        let base = || {
            let mut q = Item::query().filter(Item::GROUP_NO.eq(7));
            if extra {
                q = q.filter(Item::SCORE.lt(50.0));
            }
            q
        };
        let ordered =
            |q: memoire_orm::Select<Item>| if desc { q.order_by(Item::ID.desc()) } else { q.order_by(Item::ID.asc()) };
        let fast = ordered(base()).limit(limit).offset(offset).all(&db).await.unwrap();
        let mut all = ordered(base()).all(&db).await.unwrap();
        all = all.into_iter().skip(offset as usize).take(limit as usize).collect();
        assert_eq!(fast, all, "desc={desc} limit={limit} offset={offset} extra={extra}");
    }
    // No order: any `n` matching rows.
    let some = Item::query().filter(Item::GROUP_NO.eq(3)).limit(4).all(&db).await.unwrap();
    assert_eq!(some.len(), 4);
    assert!(some.iter().all(|i| i.group_no == 3));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `LIMIT` stops the reading early only when no aggregate needs every
/// row, wherever the aggregate stands in the projection; a limit as large as
/// can be, with an offset, is a limit all the same.
#[tokio::test]
async fn a_limit_does_not_cut_what_an_aggregate_counts() {
    use memoire_orm::ast::{Projection, SelectAst};
    use memoire_orm::{BinOp, Expr, count_all};
    let dir = temp_dir("limit");
    let db = Embedded::open_with(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    db.create_table::<Item>().await.unwrap();
    // More rows than are read at once.
    let tx = db.begin_now().unwrap();
    for n in 0..3000 {
        tx.sql(
            "INSERT INTO items (code, group_no, payload, score) VALUES (?, ?, 'p', 0.5)",
            &[format!("c{n}").into(), (n % 100).into()],
        )
        .unwrap();
    }
    tx.commit_now().unwrap();
    let mut counted = SelectAst::new(Item::SCHEMA);
    counted.projection = Projection::Exprs(vec![Expr::binary(BinOp::Add, count_all(), Expr::Value(Value::I64(2)))]);
    counted.limit = Some(1);
    assert_eq!(db.fetch_rows(Query::Select(&counted)).unwrap(), vec![vec![Value::I64(3002)]]);
    let mut rest = SelectAst::new(Item::SCHEMA);
    rest.limit = Some(u64::MAX);
    rest.offset = Some(2990);
    assert_eq!(db.fetch_rows(Query::Select(&rest)).unwrap().len(), 10);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
