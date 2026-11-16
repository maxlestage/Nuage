//! The engine: durability, crash recovery, data larger than the cache,
//! branches, backups.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use memoire_orm::prelude::*;
use memoire_orm::{Query, Value};
use nuage::{
    BackupOptions, BackupStatus, BackupStore, DirectoryStore, Embedded, Encryption, EngineOptions, Event, MirrorStore,
    PrefixedStore,
};

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
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut acknowledged = 0usize;
        let mut seen = 0;
        let mut line = String::new();
        while out.read_line(&mut line).is_ok_and(|read| read > 0) {
            if let Ok(n) = line.trim().parse::<usize>() {
                acknowledged = n;
                seen += 1;
                if seen >= 150 + round * 50 {
                    break;
                }
            }
            line.clear();
        }
        // SIGKILL: no destructor, no flush, in the middle of whatever it does.
        child.kill().unwrap();
        child.wait().unwrap();
        // The child kept committing until the kill: the groups it acknowledged meanwhile are
        // still in the pipe. Read them all, or a whole group would look like rows too many.
        // Only complete lines count: a number cut short by the kill was not acknowledged.
        line.clear();
        while out.read_line(&mut line).is_ok_and(|read| read > 0) {
            if line.ends_with('\n')
                && let Ok(n) = line.trim().parse::<usize>()
            {
                acknowledged = n;
            }
            line.clear();
        }

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

/// Every file under `dir`, by its path from there, with its bytes.
fn tree(dir: &std::path::Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.insert(name, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// Waits until a dropped database is closed: its upkeep thread lets go of it
/// a moment later, and nothing writes to its backup after that.
fn closed(engine: std::sync::Weak<nuage::Engine>) {
    for _ in 0..400 {
        if engine.upgrade().is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the database is still open");
}

/// Encrypted with the key made of `key`, backed up to `store`.
fn backed_up_to(store: Arc<dyn BackupStore>, key: u8) -> EngineOptions {
    EngineOptions {
        backup: Some(BackupOptions::new(store)),
        encryption: Some(Encryption::from_key([key; 32])),
        sync: false,
        ..EngineOptions::default()
    }
}

/// Encrypted with the key made of `key`, backed up to the folder `target`.
fn backed_up(target: &std::path::Path, key: u8) -> EngineOptions {
    backed_up_to(Arc::new(DirectoryStore::new(target)), key)
}

/// Two databases pointed at one backup folder: the second writes nothing
/// there and says why, and the first's backup still restores whole.
#[tokio::test]
async fn a_backup_is_never_written_over_another_database() {
    let (a, b) = (temp_dir("other-a"), temp_dir("other-b"));
    let (target, copy) = (temp_dir("other-backup"), temp_dir("other-copy"));
    let gone = {
        let db = Embedded::open_with(&a, backed_up(&target, 1)).unwrap();
        db.create_table::<Item>().await.unwrap();
        for n in 0..20 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().backup_now(true).unwrap();
        for n in 20..25 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    // B: a database of its own, then pointed at A's folder.
    let gone = {
        let db = Embedded::open_with(&b, EngineOptions { backup: None, ..backed_up(&target, 2) }).unwrap();
        db.create_table::<Item>().await.unwrap();
        item(1000).insert(&db).await.unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let before = tree(&target);
    assert!(before.contains_key("manifest.json") && before.contains_key("encryption.json"), "{:?}", before.keys());
    let gone = {
        let db = Embedded::open_with(&b, backed_up(&target, 2)).unwrap();
        let e = db.engine().backup_now(true).unwrap_err().to_string();
        assert!(e.contains(&format!("{} holds the backup of another database", target.display())), "{e}");
        assert!(e.contains("nothing was written there"), "{e}");
        let status = db.engine().backup_status();
        assert!(!status.last_ok && status.last_error.is_some_and(|m| m.contains("another database")));
        let (events, _) = db.engine().events();
        let told = events.iter().any(|e| e.level == "error" && e.message.starts_with("backup failed: "));
        assert!(told, "{events:?}");
        assert_eq!(Item::count(&db).await.unwrap(), 1, "B itself goes on");
        Arc::downgrade(db.engine())
    };
    closed(gone);
    assert_eq!(tree(&target), before, "not a byte of B's in A's folder, nothing of A's removed");
    // A's backup restores whole into an empty directory.
    let db = Embedded::open_with(&copy, backed_up(&target, 1)).unwrap();
    assert!(db.engine().backup_status().restored);
    assert_eq!(Item::count(&db).await.unwrap(), 25);
    assert_eq!(Item::get(&db, 25).await.unwrap().code, "code-00000024");
    drop(db);
    for d in [a, b, target, copy] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A database restored onto an empty disk is the same database: its backup
/// goes on in the same place, at the root of a folder as under an
/// application's prefix next to it.
#[tokio::test]
async fn the_same_database_continues_its_backup() {
    let target = temp_dir("same-backup");
    let root: Arc<dyn BackupStore> = Arc::new(DirectoryStore::new(&target));
    let shop: Arc<dyn BackupStore> = Arc::new(PrefixedStore::new(root.clone(), "apps/shop/"));
    for (name, store) in [("root", root), ("shop", shop)] {
        let dirs = ["", "-restored", "-again"].map(|end| temp_dir(&format!("same-{name}{end}")));
        let gone = {
            let db = Embedded::open_with(&dirs[0], backed_up_to(store.clone(), 1)).unwrap();
            db.create_table::<Item>().await.unwrap();
            for n in 0..10 {
                item(n).insert(&db).await.unwrap();
            }
            db.engine().backup_now(true).unwrap();
            Arc::downgrade(db.engine())
        };
        closed(gone);
        let gone = {
            let db = Embedded::open_with(&dirs[1], backed_up_to(store.clone(), 1)).unwrap();
            assert!(db.engine().backup_status().restored, "{name}");
            item(10).insert(&db).await.unwrap();
            db.engine().backup_now(true).unwrap();
            assert!(db.engine().backup_status().last_ok, "{name}");
            Arc::downgrade(db.engine())
        };
        closed(gone);
        // The latest backup is the restored database's.
        let db = Embedded::open_with(&dirs[2], backed_up_to(store, 1)).unwrap();
        assert!(db.engine().backup_status().restored, "{name}");
        assert_eq!(Item::count(&db).await.unwrap(), 11, "{name}");
        drop(db);
        for d in dirs {
            let _ = std::fs::remove_dir_all(&d);
        }
    }
    // Two databases in one folder, each in its own place, with its own header.
    let header = |at: &str| std::fs::read(target.join(at)).unwrap();
    assert_ne!(header("encryption.json"), header("apps/shop/encryption.json"));
    let _ = std::fs::remove_dir_all(&target);
}

/// A data directory that stays, pointed at a new, empty folder: everything is
/// there after the first round, and the old folder is left as it was.
#[tokio::test]
async fn a_new_backup_folder_gets_everything_at_the_next_start() {
    let (dir, copy) = (temp_dir("move-data"), temp_dir("move-copy"));
    let (old, new) = (temp_dir("move-old"), temp_dir("move-new"));
    let gone = {
        let db = Embedded::open_with(&dir, backed_up(&old, 1)).unwrap();
        db.create_table::<Item>().await.unwrap();
        for n in 0..20 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().backup_now(true).unwrap();
        // Written after the snapshot: they reach the old folder as log segments.
        for n in 20..30 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let before = tree(&old);
    let gone = {
        let db = Embedded::open_with(&dir, backed_up(&new, 1)).unwrap();
        assert!(!db.engine().backup_status().restored);
        db.engine().backup_now(false).unwrap();
        assert!(db.engine().backup_status().last_snapshot_at.is_some());
        Arc::downgrade(db.engine())
    };
    closed(gone);
    assert_eq!(tree(&old), before, "the old folder is left as it was");
    let db = Embedded::open_with(&copy, backed_up(&new, 1)).unwrap();
    assert!(db.engine().backup_status().restored);
    assert_eq!(Item::count(&db).await.unwrap(), 30);
    drop(db);
    for d in [dir, copy, old, new] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A folder that holds part of another database's backup (its header and a
/// snapshot, without the manifest: a copy cut short): a new database on an
/// empty disk, even with the same key, starts but writes nothing there, and
/// neither does a database in clear.
#[tokio::test]
async fn a_partial_copy_is_not_overwritten() {
    let (first, full) = (temp_dir("partial-first"), temp_dir("partial-full"));
    let (dir, clear, partial) = (temp_dir("partial-new"), temp_dir("partial-clear"), temp_dir("partial-copy"));
    let gone = {
        let db = Embedded::open_with(&first, backed_up(&full, 1)).unwrap();
        db.create_table::<Item>().await.unwrap();
        item(1).insert(&db).await.unwrap();
        db.engine().backup_now(true).unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let whole = tree(&full);
    let (snapshot, bytes) = whole.iter().find(|(name, _)| name.starts_with("snapshots/")).unwrap();
    std::fs::create_dir_all(partial.join("snapshots")).unwrap();
    std::fs::write(partial.join(snapshot), bytes).unwrap();
    std::fs::write(partial.join("encryption.json"), &whole["encryption.json"]).unwrap();
    let before = tree(&partial);
    let gone = {
        let db = Embedded::open_with(&dir, backed_up(&partial, 1)).unwrap();
        assert!(!db.engine().backup_status().restored, "no manifest: nothing to restore");
        let e = db.engine().backup_now(true).unwrap_err().to_string();
        assert!(e.contains("holds the backup of another database"), "{e}");
        Arc::downgrade(db.engine())
    };
    closed(gone);
    assert_eq!(tree(&partial), before, "both objects as they were, and nothing else");
    // A database in clear is refused there too: nothing goes from an
    // encrypted backup back to one in clear.
    let gone = {
        let db = Embedded::open_with(&clear, EngineOptions { encryption: None, ..backed_up(&partial, 1) }).unwrap();
        let e = db.engine().backup_now(true).unwrap_err().to_string();
        assert!(e.contains("holds the backup of another database"), "{e}");
        Arc::downgrade(db.engine())
    };
    closed(gone);
    assert_eq!(tree(&partial), before);
    for d in [first, full, dir, clear, partial] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The console's Back up now during the upkeep thread's round: rounds take
/// turns, so neither tears the other's copy of the page store.
#[tokio::test]
async fn two_backups_at_once_leave_a_snapshot_that_opens() {
    let (dir, target, copy) = (temp_dir("twice-data"), temp_dir("twice-backup"), temp_dir("twice-copy"));
    let gone = {
        let db = Embedded::open_with(&dir, backed_up(&target, 1)).unwrap();
        db.create_table::<Item>().await.unwrap();
        // A page store of a few megabytes: a copy of it takes a while.
        let tx = db.begin_now().unwrap();
        for n in 0..8000 {
            let it = item(n);
            tx.sql(
                "INSERT INTO items (code, group_no, payload, score) VALUES (?, ?, ?, 0.5)",
                &[it.code.into(), (n % 100).into(), it.payload.into()],
            )
            .unwrap();
        }
        tx.commit_now().unwrap();
        let engine = db.engine();
        for _ in 0..3 {
            std::thread::scope(|s| {
                let rounds = [s.spawn(|| engine.backup_now(true)), s.spawn(|| engine.backup_now(true))];
                for round in rounds {
                    round.join().unwrap().unwrap();
                }
            });
        }
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let db = Embedded::open_with(&copy, backed_up(&target, 1)).unwrap();
    assert!(db.engine().backup_status().restored);
    assert_eq!(Item::count(&db).await.unwrap(), 8000);
    assert!(db.engine().check_integrity().unwrap());
    drop(db);
    for d in [dir, target, copy] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The warning an empty start records, if any.
fn empty_start_warning(engine: &nuage::Engine) -> Option<String> {
    let (events, _) = engine.events();
    events.into_iter().find(|e| e.level == "warn" && e.message.contains("empty database")).map(|e| e.message)
}

/// An empty data directory whose backup holds nothing starts a new, empty
/// database and says so; a start that restored, or that found its data,
/// says nothing of the kind.
#[tokio::test]
async fn an_empty_start_says_so() {
    let (dir, copy, plain) = (temp_dir("empty-start"), temp_dir("empty-start-copy"), temp_dir("empty-start-plain"));
    let target = temp_dir("empty-start-backup");
    std::fs::create_dir_all(&target).unwrap();
    let gone = {
        let engine = nuage::Engine::open(&dir, backed_up(&target, 1)).unwrap();
        let status = engine.backup_status();
        assert!(status.started_empty && !status.restored, "{status:?}");
        let warning = empty_start_warning(&engine).expect("a warn event");
        assert_eq!(
            warning,
            format!(
                "the data directory was empty and {} holds no backup: this is a new, empty database",
                target.display()
            )
        );
        let db = Embedded::from_engine(engine);
        db.create_table::<Item>().await.unwrap();
        item(1).insert(&db).await.unwrap();
        db.engine().backup_now(true).unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    // Restored onto an empty disk: neither.
    let gone = {
        let engine = nuage::Engine::open(&copy, backed_up(&target, 1)).unwrap();
        let status = engine.backup_status();
        assert!(status.restored && !status.started_empty, "{status:?}");
        assert_eq!(empty_start_warning(&engine), None);
        Arc::downgrade(&engine)
    };
    closed(gone);
    // A directory that holds its data: neither.
    let engine = nuage::Engine::open(&dir, backed_up(&target, 1)).unwrap();
    let status = engine.backup_status();
    assert!(!status.restored && !status.started_empty, "{status:?}");
    assert_eq!(empty_start_warning(&engine), None);
    let gone = Arc::downgrade(&engine);
    drop(engine);
    closed(gone);
    // Without a backup, an empty directory is simply a new database: there
    // was nowhere to look.
    let engine = nuage::Engine::open(&plain, EngineOptions { backup: None, ..backed_up(&target, 1) }).unwrap();
    assert!(!engine.backup_status().started_empty && empty_start_warning(&engine).is_none());
    let gone = Arc::downgrade(&engine);
    drop(engine);
    closed(gone);
    for d in [dir, copy, plain, target] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// `Engine::open_existing`: an empty directory is restored from the backup,
/// or not opened at all. Refused, it creates nothing in the directory and
/// writes nothing to the backup.
#[tokio::test]
async fn an_empty_directory_must_be_restored_when_required() {
    let (src, dir) = (temp_dir("required-src"), temp_dir("required-dir"));
    let (empty, nowhere, full, partial) = (
        temp_dir("required-empty"),
        temp_dir("required-nowhere"),
        temp_dir("required-full"),
        temp_dir("required-partial"),
    );
    std::fs::create_dir_all(&empty).unwrap();
    let refused = |options: EngineOptions| {
        let e = nuage::Engine::open_existing(&dir, options).unwrap_err().to_string();
        assert!(!dir.exists(), "nothing was created in {}", dir.display());
        e
    };
    // An empty backup folder, and one that does not exist (a disk not plugged in).
    let e = refused(backed_up(&empty, 1));
    let expected = format!(
        "this directory is empty and {} holds no backup to restore: an empty database is not started here",
        empty.display()
    );
    assert!(e.contains(&expected), "{e}");
    assert!(tree(&empty).is_empty(), "nothing was written to the backup");
    let e = refused(backed_up(&nowhere, 1));
    assert!(e.contains("holds no backup to restore"), "{e}");
    assert!(!nowhere.exists());
    // No backup at all (its settings were removed), or restoring turned off.
    let e = refused(EngineOptions { backup: None, ..backed_up(&empty, 1) });
    assert!(e.contains("this directory is empty and no backup destination is set: an empty database"), "{e}");
    let off = BackupOptions { restore_when_empty: false, ..BackupOptions::new(Arc::new(DirectoryStore::new(&empty))) };
    let e = refused(EngineOptions { backup: Some(off), ..backed_up(&empty, 1) });
    assert!(e.contains("is turned off (`restore_when_empty`): an empty database is not started here"), "{e}");
    assert!(tree(&empty).is_empty());

    // A real backup: restored.
    let gone = {
        let db = Embedded::open_with(&src, backed_up(&full, 1)).unwrap();
        db.create_table::<Item>().await.unwrap();
        for n in 0..10 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().backup_now(true).unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    // A copy without its manifest (cut short): nothing to restore, and left as it was.
    for (name, bytes) in tree(&full).into_iter().filter(|(name, _)| name != "manifest.json") {
        let path = partial.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let before = tree(&partial);
    let e = refused(backed_up(&partial, 1));
    assert!(e.contains("holds no backup to restore"), "{e}");
    assert_eq!(tree(&partial), before);
    let gone = {
        let engine = nuage::Engine::open_existing(&dir, backed_up(&full, 1)).unwrap();
        assert!(engine.backup_status().restored && !engine.backup_status().started_empty);
        let db = Embedded::from_engine(engine);
        assert_eq!(Item::count(&db).await.unwrap(), 10);
        Arc::downgrade(db.engine())
    };
    closed(gone);
    // A directory that holds a database opens, whatever the backup holds.
    for options in [backed_up(&empty, 1), EngineOptions { backup: None, ..backed_up(&empty, 1) }] {
        let engine = nuage::Engine::open_existing(&src, options).unwrap();
        assert!(!engine.backup_status().restored && !engine.backup_status().started_empty);
        let db = Embedded::from_engine(engine);
        assert_eq!(Item::count(&db).await.unwrap(), 10);
        let gone = Arc::downgrade(db.engine());
        drop(db);
        closed(gone);
    }
    for d in [src, dir, empty, nowhere, full, partial] {
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

/// A backup folder for the tests that stops answering (`down`), refuses to
/// write (`read_only`), or holds every call until it is let go (`hang`: a
/// network folder that hangs, a bucket that says nothing), on demand.
struct TestPlace {
    folder: DirectoryStore,
    down: AtomicBool,
    read_only: AtomicBool,
    hang: AtomicBool,
    /// Calls held now.
    held: AtomicUsize,
    /// Milliseconds each snapshot takes to upload (a slow line), and the
    /// snapshot uploads begun.
    slow_snapshots: AtomicU64,
    snapshots_begun: AtomicUsize,
}

impl TestPlace {
    fn new(root: &std::path::Path) -> Arc<TestPlace> {
        Arc::new(TestPlace {
            folder: DirectoryStore::new(root),
            down: AtomicBool::new(false),
            read_only: AtomicBool::new(false),
            hang: AtomicBool::new(false),
            held: AtomicUsize::new(0),
            slow_snapshots: AtomicU64::new(0),
            snapshots_begun: AtomicUsize::new(0),
        })
    }

    /// Waits until a snapshot upload begins after `before` began.
    fn wait_snapshot(&self, before: usize) {
        for _ in 0..2000 {
            if self.snapshots_begun.load(Ordering::SeqCst) > before {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no snapshot reached {}", self.describe());
    }

    /// `root`, read and never written to, holding every call until let go.
    fn hanging(root: &std::path::Path) -> Arc<TestPlace> {
        let place = TestPlace::reading(root);
        place.hang.store(true, Ordering::SeqCst);
        place
    }

    /// Holds the calls from now (`true`), or lets them go.
    fn set_hang(&self, hang: bool) {
        self.hang.store(hang, Ordering::SeqCst);
    }

    /// Waits until a call is held.
    fn wait_held(&self) {
        for _ in 0..2000 {
            if self.held.load(Ordering::SeqCst) > 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no call reached {}", self.describe());
    }

    /// `root`, read and never written to: what the tests restore from, so
    /// that the restored database's own backups leave it as it was.
    fn reading(root: &std::path::Path) -> Arc<TestPlace> {
        let place = TestPlace::new(root);
        place.read_only.store(true, Ordering::SeqCst);
        place
    }

    /// `root`, answering nothing.
    fn away(root: &std::path::Path) -> Arc<TestPlace> {
        let place = TestPlace::new(root);
        place.set_down(true);
        place
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    fn answer(&self, write: bool) -> memoire_orm::Result<()> {
        if self.hang.load(Ordering::SeqCst) {
            self.held.fetch_add(1, Ordering::SeqCst);
            while self.hang.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.held.fetch_sub(1, Ordering::SeqCst);
        }
        let why = if self.down.load(Ordering::SeqCst) {
            "does not answer"
        } else if write && self.read_only.load(Ordering::SeqCst) {
            "is only read in this test"
        } else {
            return Ok(());
        };
        let e = std::io::Error::other(format!("{} {why}", self.folder.describe()));
        Err(memoire_orm::Error::Database(Box::new(e)))
    }
}

impl BackupStore for TestPlace {
    fn describe(&self) -> String {
        self.folder.describe()
    }

    fn put(&self, name: &str, data: &[u8]) -> memoire_orm::Result<()> {
        self.answer(true)?;
        self.folder.put(name, data)
    }

    fn put_file(&self, name: &str, path: &std::path::Path) -> memoire_orm::Result<()> {
        self.answer(true)?;
        if name.starts_with("snapshots/") {
            // Read at once, as an upload does, then on the line for a while.
            let data = std::fs::read(path).unwrap();
            self.snapshots_begun.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(self.slow_snapshots.load(Ordering::SeqCst)));
            return self.folder.put(name, &data);
        }
        self.folder.put_file(name, path)
    }

    fn get(&self, name: &str) -> memoire_orm::Result<Option<Vec<u8>>> {
        self.answer(false)?;
        self.folder.get(name)
    }

    fn list(&self, prefix: &str) -> memoire_orm::Result<Vec<String>> {
        self.answer(false)?;
        self.folder.list(prefix)
    }

    fn delete(&self, name: &str) -> memoire_orm::Result<()> {
        self.answer(true)?;
        self.folder.delete(name)
    }
}

/// A backup folder that writes down every call made to it (the LSNs in the
/// names as `<lsn>`).
struct Recorded {
    folder: DirectoryStore,
    calls: Mutex<Vec<String>>,
}

impl Recorded {
    fn note(&self, call: &str, name: &str) {
        let name: Vec<String> = name
            .split('/')
            .map(|part| match part.split_once('.') {
                Some((lsn, end)) if lsn.len() == 20 && lsn.bytes().all(|b| b.is_ascii_digit()) => {
                    format!("<lsn>.{end}")
                }
                _ => part.to_owned(),
            })
            .collect();
        self.calls.lock().unwrap().push(format!("{call} {}", name.join("/")));
    }

    /// The calls since the last time.
    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl BackupStore for Recorded {
    fn describe(&self) -> String {
        self.folder.describe()
    }

    fn put(&self, name: &str, data: &[u8]) -> memoire_orm::Result<()> {
        self.note("PUT", name);
        self.folder.put(name, data)
    }

    fn put_file(&self, name: &str, path: &std::path::Path) -> memoire_orm::Result<()> {
        self.note("PUT", name);
        self.folder.put_file(name, path)
    }

    fn get(&self, name: &str) -> memoire_orm::Result<Option<Vec<u8>>> {
        self.note("GET", name);
        self.folder.get(name)
    }

    fn list(&self, prefix: &str) -> memoire_orm::Result<Vec<String>> {
        self.note("LIST", prefix);
        self.folder.list(prefix)
    }

    fn delete(&self, name: &str) -> memoire_orm::Result<()> {
        self.note("DELETE", name);
        self.folder.delete(name)
    }
}

/// Both places, as one store.
fn mirror(first: Arc<dyn BackupStore>, second: Arc<dyn BackupStore>) -> Arc<dyn BackupStore> {
    Arc::new(MirrorStore::new(first, second))
}

/// Waits for the backup round that a database just opened makes at once (its
/// upkeep thread's) to have tried its `destinations`: the tests make theirs
/// after it, one at a time.
fn first_round(engine: &nuage::Engine, destinations: usize) {
    for _ in 0..2000 {
        let tried = engine
            .events()
            .0
            .iter()
            .filter(|e| e.message.starts_with("backup snapshot sent to ") || e.message.starts_with("backup failed: "))
            .count();
        if tried >= destinations {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("no backup round");
}

/// The log segments in a data directory.
fn segments_in(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir.join("wal"))
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".log"))
        .count()
}

/// What an empty directory gets from `store`: the status of its backup, its
/// events, its rows of `items` (`-1`: no such table) and its branches.
async fn restored_from(store: Arc<dyn BackupStore>, name: &str) -> (BackupStatus, Vec<Event>, i64, usize) {
    let dir = temp_dir(name);
    let db = Embedded::open_with(&dir, backed_up_to(store, 1)).unwrap();
    let rows = Item::count(&db).await.map_or(-1, |n| n as i64);
    let found = (db.engine().backup_status(), db.engine().events().0, rows, db.engine().branches().len());
    let gone = Arc::downgrade(db.engine());
    drop(db);
    closed(gone);
    let _ = std::fs::remove_dir_all(&dir);
    found
}

/// The LSN of the latest snapshot in a backup folder.
fn snapshot_lsn(folder: &std::path::Path) -> u64 {
    nuage::engine::backup::inspect(&DirectoryStore::new(folder)).unwrap().expect("a backup").lsn
}

/// Two destinations get the same backup, from one copy of the page store:
/// each one alone restores every row and branch into an empty directory.
#[tokio::test]
async fn two_destinations_hold_the_same_backup() {
    let (dir, one, two) = (temp_dir("two-data"), temp_dir("two-one"), temp_dir("two-two"));
    let places = mirror(Arc::new(DirectoryStore::new(&one)), Arc::new(DirectoryStore::new(&two)));
    let gone = {
        let db = Embedded::open_with(&dir, backed_up_to(places, 1)).unwrap();
        first_round(db.engine(), 2);
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
        assert!(status.last_ok && status.last_error.is_none(), "{status:?}");
        assert_eq!(status.target, Some(format!("{} and {}", one.display(), two.display())));
        let sent: Vec<String> = db.engine().events().0.into_iter().map(|e| e.message).collect();
        for place in [&one, &two] {
            let to = format!("backup snapshot sent to {}", place.display());
            assert_eq!(sent.iter().filter(|m| **m == to).count(), 2, "{sent:?}");
        }
        Arc::downgrade(db.engine())
    };
    closed(gone);
    // One copy for both: the same snapshots (byte for byte) and the same log segments.
    let (a, b) = (tree(&one), tree(&two));
    assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
    assert!(a.keys().any(|k| k.starts_with("wal/")), "{:?}", a.keys());
    for (name, bytes) in a.iter().filter(|(name, _)| !name.ends_with("manifest.json")) {
        assert_eq!(&b[name], bytes, "{name}");
    }
    for place in [&one, &two] {
        let (status, _, rows, branches) = restored_from(TestPlace::reading(place), "two-restored").await;
        assert!(status.restored, "{}", place.display());
        assert_eq!((rows, branches), (30, 1), "{}", place.display());
    }
    for d in [dir, one, two] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// With one destination, the backup store gets the same calls, in the same
/// order, as it always has: the restore's look at an empty folder, then
/// each round's.
#[tokio::test]
async fn one_destination_alone_sends_as_before() {
    let (dir, folder) = (temp_dir("calls-data"), temp_dir("calls-backup"));
    let recorded = Arc::new(Recorded { folder: DirectoryStore::new(&folder), calls: Mutex::new(Vec::new()) });
    let snapshot = [
        "GET encryption.json",
        "GET manifest.json",
        "PUT snapshots/<lsn>.db",
        "PUT encryption.json",
        "PUT manifest.json",
        "LIST snapshots/",
    ];
    let gone = {
        let db = Embedded::open_with(&dir, backed_up_to(recorded.clone(), 1)).unwrap();
        first_round(db.engine(), 1);
        // The restore found nothing, then the first round sent a snapshot.
        assert_eq!(recorded.calls(), [&["GET manifest.json"][..], &snapshot].concat());
        db.create_table::<Item>().await.unwrap();
        for n in 0..2 {
            item(n).insert(&db).await.unwrap();
            db.engine().checkpoint().unwrap();
            db.engine().backup_now(false).unwrap();
            assert_eq!(recorded.calls(), ["PUT wal/<lsn>.log"]);
        }
        db.engine().backup_now(true).unwrap();
        assert_eq!(recorded.calls(), snapshot);
        // A third snapshot: the oldest goes, with the segment that only it needed.
        item(2).insert(&db).await.unwrap();
        db.engine().backup_now(true).unwrap();
        let pruned = ["DELETE snapshots/<lsn>.db", "LIST wal/", "DELETE wal/<lsn>.log"];
        assert_eq!(recorded.calls(), [&snapshot[..], &pruned].concat());
        Arc::downgrade(db.engine())
    };
    closed(gone);
    for d in [dir, folder] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A second destination that does not answer never holds the first back:
/// segments keep reaching the first, the status says what fails, and the
/// data directory keeps only what the first needs. And the reverse: a first
/// that does not answer leaves the second going on.
#[tokio::test]
async fn a_failing_second_does_not_hold_the_first() {
    let dir = temp_dir("failing-data");
    let (a, b) = (temp_dir("failing-one"), temp_dir("failing-two"));
    let (one, two) = (TestPlace::new(&a), TestPlace::new(&b));
    let gone = {
        let db = Embedded::open_with(&dir, backed_up_to(mirror(one.clone(), two.clone()), 1)).unwrap();
        let engine = db.engine();
        first_round(engine, 2);
        two.set_down(true);
        let before = tree(&b);
        db.create_table::<Item>().await.unwrap();
        for round in 0..5 {
            for n in 0..10 {
                item(round * 10 + n).insert(&db).await.unwrap();
            }
            let lsn = engine.checkpoint().unwrap();
            let e = engine.backup_now(false).unwrap_err().to_string();
            assert!(e.contains(&format!("second destination ({}): ", b.display())), "{e}");
            let status = engine.backup_status();
            assert_eq!(status.shipped_lsn, lsn, "the first has everything");
            assert!(!status.last_ok, "{status:?}");
            let error = status.last_error.unwrap();
            assert!(error.contains(&format!("second destination ({}): {} does not answer", b.display(), b.display())));
            assert!(segments_in(&dir) <= 2, "the data directory keeps only what the first needs");
        }
        let failed = engine.events().0.iter().filter(|e| e.message.contains("backup failed: ")).count();
        assert!(failed >= 5, "each failure is an event");
        assert_eq!(tree(&b), before, "nothing reached the second");

        // The reverse: the first stops answering, the second comes back.
        one.set_down(true);
        two.set_down(false);
        for round in 5..8 {
            for n in 0..10 {
                item(round * 10 + n).insert(&db).await.unwrap();
            }
            engine.checkpoint().unwrap();
            let e = engine.backup_now(false).unwrap_err().to_string();
            assert!(
                e.contains(&format!("{} does not answer", a.display())) && !e.contains("second destination"),
                "{e}"
            );
        }
        let status = engine.backup_status();
        assert!(!status.last_ok && !status.last_error.unwrap().contains("second destination"));
        // Kept on this disk for the first, which is behind.
        assert!(segments_in(&dir) >= 3, "{}", segments_in(&dir));
        let (_, _, rows, _) = restored_from(TestPlace::reading(&b), "failing-from-two").await;
        assert_eq!(rows, 80, "the second has every row");
        let (_, _, rows, _) = restored_from(TestPlace::reading(&a), "failing-from-one").await;
        assert_eq!(rows, 50, "the first has what it got before it stopped answering");

        // The first answers again: what it missed is still on this disk, and goes as segments.
        let to_one = format!("backup snapshot sent to {}", a.display());
        let snapshots = |engine: &nuage::Engine| engine.events().0.iter().filter(|e| e.message == to_one).count();
        let before = snapshots(engine);
        one.set_down(false);
        engine.backup_now(false).unwrap();
        let status = engine.backup_status();
        assert!(status.last_ok && status.last_error.is_none(), "{status:?}");
        assert_eq!(snapshots(engine), before, "no snapshot: the segments were there");
        engine.checkpoint().unwrap();
        assert!(segments_in(&dir) <= 2, "{}", segments_in(&dir));
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let (_, _, rows, _) = restored_from(TestPlace::reading(&a), "failing-again").await;
    assert_eq!(rows, 80);
    for d in [dir, a, b] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A second destination that missed log segments no longer on the disk
/// gets a whole snapshot when it answers again, then segments: never a
/// segment after a hole. Restored alone, it has every row.
#[tokio::test]
async fn a_second_that_was_away_comes_back_with_a_snapshot() {
    let dir = temp_dir("away-data");
    let (a, b) = (temp_dir("away-one"), temp_dir("away-two"));
    let two = TestPlace::new(&b);
    let gone = {
        let db = Embedded::open_with(&dir, backed_up_to(mirror(TestPlace::new(&a), two.clone()), 1)).unwrap();
        let engine = db.engine();
        first_round(engine, 2);
        db.create_table::<Item>().await.unwrap();
        let mut n = 0;
        let mut write = async |rows: i64| {
            for _ in 0..rows {
                item(n).insert(&db).await.unwrap();
                n += 1;
            }
        };
        write(10).await;
        engine.checkpoint().unwrap();
        engine.backup_now(false).unwrap();
        // Away for two checkpoints: the segments it misses leave the disk.
        two.set_down(true);
        let before = tree(&b);
        for _ in 0..2 {
            write(10).await;
            engine.checkpoint().unwrap();
            assert!(engine.backup_now(false).is_err());
        }
        assert_eq!(tree(&b), before, "nothing reached it while it was away");
        // Back: a snapshot first, and no segment.
        two.set_down(false);
        write(10).await;
        let sent = |engine: &nuage::Engine| {
            let to = format!("backup snapshot sent to {}", b.display());
            engine.events().0.iter().filter(|e| e.message == to).count()
        };
        let snapshots = sent(engine);
        engine.backup_now(false).unwrap();
        assert_eq!(sent(engine), snapshots + 1, "a snapshot");
        assert!(engine.backup_status().last_ok);
        let lsn = snapshot_lsn(&b);
        let segments = |tree: &BTreeMap<String, Vec<u8>>| -> Vec<u64> {
            tree.keys()
                .filter_map(|k| k.strip_prefix("wal/")?.strip_suffix(".log")?.parse().ok())
                .filter(|first| !before.contains_key(&format!("wal/{first:020}.log")))
                .collect()
        };
        assert_eq!(segments(&tree(&b)), Vec::<u64>::new(), "no segment came with the snapshot");
        // Then segments, from the snapshot on.
        write(10).await;
        engine.checkpoint().unwrap();
        engine.backup_now(false).unwrap();
        assert_eq!(sent(engine), snapshots + 1, "no other snapshot");
        assert_eq!(segments(&tree(&b)), [lsn + 1]);
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let (status, events, rows, _) = restored_from(TestPlace::reading(&b), "away-restored").await;
    assert!(status.restored);
    assert_eq!(rows, 50);
    assert!(!events.iter().any(|e| e.level == "error" && e.message.contains("the log holds commit")), "{events:?}");
    for d in [dir, a, b] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// An empty directory is restored from one destination, whole: the one that
/// reaches furthest, the first on a tie, the second when the first holds
/// nothing. Two databases, or a destination that does not answer, whichever
/// it is: nothing is restored, and nothing is created. A second that hangs
/// stops the start within seconds of the first's answer.
#[tokio::test]
async fn an_empty_directory_is_restored_from_the_one_that_reaches_furthest() {
    let (dir, other) = (temp_dir("furthest-data"), temp_dir("furthest-other"));
    let (a, b, c, empty) =
        (temp_dir("furthest-one"), temp_dir("furthest-two"), temp_dir("furthest-three"), temp_dir("furthest-empty"));
    std::fs::create_dir_all(&empty).unwrap();
    let one = TestPlace::new(&a);
    let gone = {
        let db = Embedded::open_with(&dir, backed_up_to(mirror(one.clone(), TestPlace::new(&b)), 1)).unwrap();
        first_round(db.engine(), 2);
        db.create_table::<Item>().await.unwrap();
        for n in 0..10 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let both = |first: Arc<dyn BackupStore>, second: Arc<dyn BackupStore>| mirror(first, second);
    let reading = |root: &std::path::Path| -> Arc<dyn BackupStore> { TestPlace::reading(root) };
    // In step: the first.
    let (status, _, rows, _) = restored_from(both(reading(&a), reading(&b)), "furthest-tie").await;
    assert_eq!((status.restored_from, rows), (Some(a.display().to_string()), 10));

    // The second one segment ahead: the second.
    let gone = {
        let db = Embedded::open_with(&dir, backed_up_to(mirror(one.clone(), TestPlace::new(&b)), 1)).unwrap();
        first_round(db.engine(), 2);
        one.set_down(true);
        item(10).insert(&db).await.unwrap();
        db.engine().checkpoint().unwrap();
        assert!(db.engine().backup_now(false).is_err());
        Arc::downgrade(db.engine())
    };
    closed(gone);
    one.set_down(false);
    let (status, _, rows, _) = restored_from(both(reading(&a), reading(&b)), "furthest-ahead").await;
    assert_eq!((status.restored_from, rows), (Some(b.display().to_string()), 11));
    let reach = |root: &std::path::Path| nuage::engine::backup::inspect(&DirectoryStore::new(root)).unwrap().unwrap();
    assert!(reach(&b).reach > reach(&a).reach && reach(&b).header == reach(&a).header);

    // The first holds nothing: the second.
    let (status, _, rows, _) = restored_from(both(reading(&empty), reading(&b)), "furthest-empty-first").await;
    assert_eq!((status.restored_from, rows), (Some(b.display().to_string()), 11));
    assert!(tree(&empty).is_empty());

    // Errors, and nothing made.
    let refused = |places: Arc<dyn BackupStore>| {
        let target = temp_dir("furthest-refused");
        let e = Embedded::open_with(&target, backed_up_to(places, 1)).unwrap_err().to_string();
        assert!(!target.exists(), "nothing was made: {e}");
        e
    };
    // Another database, with the same key.
    let gone = {
        let db = Embedded::open_with(&other, backed_up(&c, 1)).unwrap();
        first_round(db.engine(), 1);
        Arc::downgrade(db.engine())
    };
    closed(gone);
    let e = refused(both(reading(&a), reading(&c)));
    let expected = format!(
        "the two backup destinations hold different databases ({} and {}): nothing was restored",
        a.display(),
        c.display()
    );
    assert!(e.contains(&expected), "{e}");
    let e = refused(both(TestPlace::away(&a), reading(&b)));
    let expected =
        format!("the first backup destination does not answer ({}: {} does not answer)", a.display(), a.display());
    assert!(e.contains(&expected) && e.contains("To start from the second alone"), "{e}");
    // The second does not answer: it may be the one that reaches furthest, so
    // the first is not restored in its place, whether or not it holds a backup.
    let said =
        format!("the second backup destination does not answer ({}: {} does not answer)", b.display(), b.display());
    for first in [&a, &empty] {
        let e = refused(both(reading(first), TestPlace::away(&b)));
        assert!(e.contains(&said) && e.contains("nothing was restored"), "{e}");
        assert!(e.contains(&format!("To start from the first alone ({}), take the second out", first.display())));
    }
    // A second that hangs: the same, a few seconds after the first has answered.
    let hanging = TestPlace::hanging(&b);
    let started = std::time::Instant::now();
    let e = refused(both(reading(&a), hanging.clone()));
    assert!(e.contains("no answer 5 seconds after the first's") && e.contains("nothing was restored"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(9), "{:?}", started.elapsed());
    hanging.set_hang(false);
    for d in [dir, other, a, b, c, empty] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Moving the backups without a stop, on a disk erased at each restart: the
/// second destination is added (the restart restores from the first), the
/// server runs until the second has everything, a clean stop sends the last
/// commits to both, and the next start, with the second alone, restores
/// every commit.
#[tokio::test]
async fn adding_then_removing_a_destination_loses_nothing() {
    let (old, new) = (temp_dir("switch-old"), temp_dir("switch-new"));
    let disks = ["switch-first", "switch-both", "switch-last"].map(temp_dir);
    let place = |root: &std::path::Path| -> Arc<dyn BackupStore> { Arc::new(DirectoryStore::new(root)) };
    // The first destination alone.
    let gone = {
        let db = Embedded::open_with(&disks[0], backed_up(&old, 1)).unwrap();
        first_round(db.engine(), 1);
        db.create_table::<Item>().await.unwrap();
        for n in 0..10 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().checkpoint().unwrap();
        db.engine().backup_to(0, false).unwrap();
        db.engine().close().unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    // The second is added: an erased disk restores from the first, the only one that holds a backup.
    let gone = {
        let db = Embedded::open_with(&disks[1], backed_up_to(mirror(place(&old), place(&new)), 1)).unwrap();
        assert_eq!(db.engine().backup_status().restored_from, Some(old.display().to_string()));
        first_round(db.engine(), 2);
        for n in 10..20 {
            item(n).insert(&db).await.unwrap();
        }
        db.engine().checkpoint().unwrap();
        db.engine().backup_now(false).unwrap();
        // The second is complete: the status shows no error.
        let status = db.engine().backup_status();
        assert!(status.last_ok && status.last_at.is_some(), "{status:?}");
        // Written in the last seconds, before the stop.
        for n in 20..25 {
            item(n).insert(&db).await.unwrap();
        }
        // The stop of the server: the first destination, then the second.
        db.engine().checkpoint().unwrap();
        db.engine().backup_to(0, false).unwrap();
        db.engine().backup_to(1, false).unwrap();
        db.engine().close().unwrap();
        assert!(db.engine().backup_to(2, false).is_err(), "there are two");
        Arc::downgrade(db.engine())
    };
    closed(gone);
    // The first is removed: an erased disk restores from the second alone.
    let db = Embedded::open_with(&disks[2], backed_up(&new, 1)).unwrap();
    assert_eq!(db.engine().backup_status().restored_from, Some(new.display().to_string()));
    assert_eq!(Item::count(&db).await.unwrap(), 25);
    assert_eq!(Item::get(&db, 25).await.unwrap().code, "code-00000024", "the last commit is there");
    let gone = Arc::downgrade(db.engine());
    drop(db);
    closed(gone);
    for d in disks.into_iter().chain([old, new]) {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Waits until `done` holds (10 seconds at most).
fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..2000 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{what}: not after 10 seconds");
}

/// `f` on a thread of its own: its result, or `None` when it has not ended
/// after `limit` (the thread is left to end on its own).
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sent, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sent.send(f());
    });
    done.recv_timeout(limit).ok()
}

/// A second destination that hangs (a network folder that never answers, a
/// bucket that says nothing) never holds the first back. While the upkeep's
/// round to it is stuck, the upkeep keeps checkpointing and shipping to the
/// first by itself, a round to the first ends at once, and so does the stop
/// (no commit accepted any more, a checkpoint, the first): the first alone
/// then restores every commit acknowledged. Meanwhile the status gives no
/// time of a complete backup, since the second has nothing of this run.
/// Once it answers again, its round goes on with the log segments the
/// checkpoints kept for it, and it restores every commit too.
#[tokio::test]
async fn a_second_that_hangs_never_holds_the_first_nor_the_stop() {
    let dir = temp_dir("hang-data");
    let (a, b) = (temp_dir("hang-one"), temp_dir("hang-two"));
    let two = TestPlace::new(&b);
    let places = || mirror(TestPlace::new(&a), two.clone());
    let options = |store| EngineOptions { checkpoint_every: Duration::from_millis(50), ..backed_up_to(store, 1) };
    // Both destinations hold the database's backup.
    let gone = {
        let db = Embedded::open_with(&dir, options(places())).unwrap();
        first_round(db.engine(), 2);
        db.create_table::<Item>().await.unwrap();
        db.engine().backup_now(false).unwrap();
        db.engine().close().unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);

    // The next start: the round to the second hangs.
    two.set_hang(true);
    let db = Embedded::open_with(&dir, options(places())).unwrap();
    let engine = db.engine().clone();
    first_round(&engine, 1);
    two.wait_held();
    let status = engine.backup_status();
    assert!(status.last_ok && status.last_at.is_none(), "the second has nothing of this run yet: {status:?}");
    let [first, second] = &status.destinations[..] else { panic!("{status:?}") };
    assert!(first.last_snapshot_at.is_some() && first.last_at.is_some(), "{status:?}");
    assert!(second.last_snapshot_at.is_none() && second.last_at.is_none() && second.error.is_none(), "{status:?}");
    // The upkeep ships to the first by itself, and a round to it ends at once.
    for n in 0..20 {
        item(n).insert(&db).await.unwrap();
    }
    let lsn = engine.stats().lsn;
    eventually("the upkeep ships to the first", || engine.backup_status().shipped_lsn >= lsn);
    let first_only = engine.clone();
    let shipped = within(Duration::from_secs(5), move || first_only.backup_to(0, true));
    assert!(shipped.expect("a round to the first waits for the second").is_ok());

    // It answers again: its round ends, and the status has a time.
    two.set_hang(false);
    eventually("the second gets its snapshot", || engine.backup_status().last_at.is_some());
    assert!(engine.backup_status().destinations[1].last_snapshot_at.is_some());

    // It hangs while it is sent log segments: the checkpoints that follow keep
    // what it has yet to send, and it goes on with them once it answers.
    two.set_hang(true);
    for n in 20..30 {
        item(n).insert(&db).await.unwrap();
    }
    two.wait_held();
    for n in 30..40 {
        item(n).insert(&db).await.unwrap();
    }
    let lsn = engine.stats().lsn;
    eventually("the first gets everything", || engine.backup_status().shipped_lsn >= lsn);
    let failed_before = engine.events().0.iter().filter(|e| e.message.starts_with("backup failed")).count();
    two.set_hang(false);
    // After the round that was stuck, one more: nothing was missing for either.
    engine.backup_to(1, false).unwrap();
    assert!(engine.backup_status().destinations[1].shipped_lsn >= lsn, "{:?}", engine.backup_status());
    let failed = engine.events().0.iter().filter(|e| e.message.starts_with("backup failed")).count();
    assert_eq!(failed, failed_before, "{:?}", engine.events().0);

    // The stop while the second hangs again.
    two.set_hang(true);
    for n in 40..45 {
        item(n).insert(&db).await.unwrap();
    }
    two.wait_held();
    let stopping = engine.clone();
    let stopped = within(Duration::from_secs(5), move || {
        stopping.stop_writes();
        stopping.checkpoint()?;
        stopping.backup_to(0, false)
    });
    assert!(stopped.expect("the stop waits for the second").is_ok());
    // A commit that comes after is refused, and nothing is written.
    let e = item(45).insert(&db).await.unwrap_err().to_string();
    assert!(e.contains("the database is stopping: nothing was written"), "{e}");
    assert_eq!(Item::count(&db).await.unwrap(), 45, "reads go on");
    let (_, _, rows, _) = restored_from(TestPlace::reading(&a), "hang-from-one").await;
    assert_eq!(rows, 45, "the first has every commit acknowledged");
    two.set_hang(false);
    engine.backup_to(1, false).unwrap();
    let gone = Arc::downgrade(&engine);
    drop((db, engine));
    closed(gone);
    let (_, events, rows, _) = restored_from(TestPlace::reading(&b), "hang-from-two").await;
    assert_eq!(rows, 45);
    assert!(!events.iter().any(|e| e.message.contains("the log holds commit")), "{events:?}");
    for d in [dir, a, b] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The copies of the page store a process left when it stopped during a
/// snapshot are removed at the next opening; the data stays.
#[tokio::test]
async fn copies_left_by_a_stop_are_removed() {
    let (dir, folder) = (temp_dir("copies-data"), temp_dir("copies-backup"));
    let gone = {
        let db = Embedded::open_with(&dir, backed_up(&folder, 1)).unwrap();
        db.create_table::<Item>().await.unwrap();
        item(1).insert(&db).await.unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);
    for name in ["snapshot-7.partial", "snapshot.partial"] {
        std::fs::write(dir.join(name), b"half a copy").unwrap();
    }
    // The last handle gone, its files may be let go a moment later (the end of its drop).
    let mut tries = 0;
    let db = loop {
        match Embedded::open_with(&dir, backed_up(&folder, 1)) {
            Ok(db) => break db,
            Err(e) if tries < 200 && e.to_string().contains("already open") => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("{e}"),
        }
    };
    for name in ["snapshot-7.partial", "snapshot.partial"] {
        assert!(!dir.join(name).exists(), "{name}");
    }
    assert_eq!(Item::count(&db).await.unwrap(), 1);
    let gone = Arc::downgrade(db.engine());
    drop(db);
    closed(gone);
    for d in [dir, folder] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// The reverse of a second that hangs: a first that hangs (a bucket that
/// takes the connection and says nothing, a network folder) never holds the
/// second back. While the upkeep's round to it is stuck, the upkeep keeps
/// checkpointing and shipping to the second by itself, a round to the second
/// ends at once, and so does its part of the stop: the second alone then
/// restores every commit acknowledged. Once the first answers again, it gets
/// everything too.
#[tokio::test]
async fn a_first_that_hangs_never_holds_the_second_nor_the_stop() {
    let dir = temp_dir("first-hang-data");
    let (a, b) = (temp_dir("first-hang-one"), temp_dir("first-hang-two"));
    let one = TestPlace::new(&a);
    let places = || mirror(one.clone(), TestPlace::new(&b));
    let options = |store| EngineOptions { checkpoint_every: Duration::from_millis(50), ..backed_up_to(store, 1) };
    let gone = {
        let db = Embedded::open_with(&dir, options(places())).unwrap();
        first_round(db.engine(), 2);
        db.create_table::<Item>().await.unwrap();
        db.engine().backup_now(false).unwrap();
        db.engine().close().unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);

    // The next start: the round to the first hangs.
    one.set_hang(true);
    let db = Embedded::open_with(&dir, options(places())).unwrap();
    let engine = db.engine().clone();
    one.wait_held();
    let checkpoint = engine.stats().checkpoint_lsn;
    for n in 0..20 {
        item(n).insert(&db).await.unwrap();
    }
    let lsn = engine.stats().lsn;
    eventually("the upkeep ships to the second", || engine.backup_status().destinations[1].shipped_lsn >= lsn);
    assert!(engine.stats().checkpoint_lsn > checkpoint, "the checkpoints go on");
    let second = engine.backup_status().destinations[1].clone();
    assert!(second.last_snapshot_at.is_some() && second.error.is_none(), "{second:?}");
    let second_only = engine.clone();
    let shipped = within(Duration::from_secs(5), move || second_only.backup_to(1, true));
    assert!(shipped.expect("a round to the second waits for the first").is_ok());

    // The stop while the first hangs: its pass waits, the second's ends.
    for n in 20..25 {
        item(n).insert(&db).await.unwrap();
    }
    let stopping = engine.clone();
    let stopped = within(Duration::from_secs(5), move || {
        stopping.stop_writes();
        stopping.checkpoint()?;
        let first = stopping.clone();
        std::thread::spawn(move || first.backup_to(0, false));
        stopping.backup_to(1, false)
    });
    assert!(stopped.expect("the stop's second pass waits for the first").is_ok());
    let (_, _, rows, _) = restored_from(TestPlace::reading(&b), "first-hang-from-two").await;
    assert_eq!(rows, 25, "the second has every commit acknowledged");
    one.set_hang(false);
    engine.backup_to(0, false).unwrap();
    let gone = Arc::downgrade(&engine);
    drop((db, engine));
    closed(gone);
    let (_, events, rows, _) = restored_from(TestPlace::reading(&a), "first-hang-from-one").await;
    assert_eq!(rows, 25);
    assert!(!events.iter().any(|e| e.message.contains("the log holds commit")), "{events:?}");
    for d in [dir, a, b] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Backed up to `store`, checkpointing often, a round that has not ended
/// after a second being a failure.
fn quick_to_say(store: Arc<dyn BackupStore>) -> EngineOptions {
    let mut options = EngineOptions { checkpoint_every: Duration::from_millis(50), ..backed_up_to(store, 1) };
    options.backup.as_mut().unwrap().stuck_after = Duration::from_secs(1);
    options
}

/// A round that does not end (a network folder that stops answering, for
/// good) is a failure of its destination once it has run longer than
/// `stuck_after`: the status says so, and an `error` event says it once,
/// even when the destination had no snapshot of this run yet; the time of
/// the last complete backup stops moving. Meanwhile Back up now sends to the
/// other destination and says, within seconds, that a round to this one is
/// still running, instead of waiting behind it. Once the round ends, the
/// status is right again. The same for the first destination.
#[tokio::test]
async fn a_round_that_does_not_end_is_a_failure() {
    let dir = temp_dir("stuck-data");
    let (a, b) = (temp_dir("stuck-one"), temp_dir("stuck-two"));
    let (one, two) = (TestPlace::new(&a), TestPlace::new(&b));
    let places = || mirror(one.clone(), two.clone());
    let gone = {
        let db = Embedded::open_with(&dir, quick_to_say(places())).unwrap();
        first_round(db.engine(), 2);
        db.create_table::<Item>().await.unwrap();
        db.engine().backup_now(false).unwrap();
        db.engine().close().unwrap();
        Arc::downgrade(db.engine())
    };
    closed(gone);

    // The next start: the second hangs before its first snapshot of this run.
    two.set_hang(true);
    let db = Embedded::open_with(&dir, quick_to_say(places())).unwrap();
    let engine = db.engine().clone();
    two.wait_held();
    assert!(engine.backup_status().last_ok, "not yet: {:?}", engine.backup_status());
    eventually("the status says the round does not end", || !engine.backup_status().last_ok);
    let said = format!("second destination ({}): a backup round has been running for ", b.display());
    let status = engine.backup_status();
    assert!(status.destinations[1].error.as_deref().is_some_and(|e| e.contains(&said)), "{status:?}");
    assert!(status.last_error.as_deref().is_some_and(|e| e.contains(&said)), "{status:?}");
    assert!(status.destinations[0].error.is_none() && status.last_at.is_none(), "{status:?}");
    // The first goes on; the status still says the second fails.
    for n in 0..10 {
        item(n).insert(&db).await.unwrap();
    }
    let lsn = engine.stats().lsn;
    eventually("the first gets everything", || engine.backup_status().shipped_lsn >= lsn);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!engine.backup_status().last_ok && engine.backup_status().last_at.is_none());
    let reported = |engine: &nuage::Engine, said: &str| {
        engine.events().0.iter().filter(|e| e.level == "error" && e.message.contains(said)).count()
    };
    assert_eq!(reported(&engine, &said), 1, "said once: {:?}", engine.events().0);
    // Back up now: the first gets its snapshot, and the second is said busy.
    let to_one = format!("backup snapshot sent to {}", a.display());
    let snapshots = engine.events().0.iter().filter(|e| e.message == to_one).count();
    let now = engine.clone();
    let started = std::time::Instant::now();
    let e = within(Duration::from_secs(30), move || now.backup_now_within(true, Some(Duration::from_secs(10))))
        .expect("Back up now waits behind the second's round")
        .unwrap_err()
        .to_string();
    let busy = format!("second destination ({}): a backup round to it has been running for ", b.display());
    assert!(e.contains(&busy) && e.contains("try again later"), "{e}");
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    assert_eq!(engine.events().0.iter().filter(|e| e.message == to_one).count(), snapshots + 1);

    // It answers again: the round ends, and the status is right.
    two.set_hang(false);
    eventually("the round ends", || {
        let s = engine.backup_status();
        s.last_ok && s.last_at.is_some() && s.destinations[1].error.is_none()
    });

    // The first hangs.
    one.set_hang(true);
    item(10).insert(&db).await.unwrap();
    one.wait_held();
    let said = format!("{}: a backup round has been running for ", a.display());
    eventually("the status says the first's round does not end", || {
        engine.backup_status().destinations[0].error.as_deref().is_some_and(|e| e.contains(&said))
    });
    let status = engine.backup_status();
    assert!(!status.last_ok && !status.last_error.unwrap().contains("second destination"));
    one.set_hang(false);
    eventually("the first's round ends", || engine.backup_status().last_ok);
    let gone = Arc::downgrade(&engine);
    drop((db, engine));
    closed(gone);
    for d in [dir, a, b] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A snapshot to the second destination that takes long to upload (a large
/// database on a slow line) while commits and checkpoints go on: the log
/// after it stays on this disk for as long as the upload takes, though the
/// first already has it, so the round ends with the segments after its
/// snapshot, and the next one sends segments, not another snapshot. The
/// time of the last complete backup moves.
#[tokio::test]
async fn a_long_snapshot_to_the_second_keeps_the_log_it_needs() {
    let dir = temp_dir("long-snapshot-data");
    let (a, b) = (temp_dir("long-snapshot-one"), temp_dir("long-snapshot-two"));
    let two = TestPlace::new(&b);
    let options = EngineOptions {
        checkpoint_every: Duration::from_millis(50),
        ..backed_up_to(mirror(TestPlace::new(&a), two.clone()), 1)
    };
    let db = Embedded::open_with(&dir, options).unwrap();
    let engine = db.engine().clone();
    first_round(&engine, 2);
    db.create_table::<Item>().await.unwrap();
    let to_two = format!("backup snapshot sent to {}", b.display());
    let snapshots = |engine: &nuage::Engine| engine.events().0.iter().filter(|e| e.message == to_two).count();
    two.slow_snapshots.store(2500, Ordering::SeqCst);
    let before = two.snapshots_begun.load(Ordering::SeqCst);
    let sending = engine.clone();
    let round = std::thread::spawn(move || sending.backup_to(1, true));
    two.wait_snapshot(before);
    // Meanwhile, up to its end: commits, checkpoints, and the first gets every segment.
    let mut n = 0;
    while !round.is_finished() {
        item(n).insert(&db).await.unwrap();
        n += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
    round.join().unwrap().expect("the log after the snapshot was kept");
    let lsn = engine.stats().lsn;
    eventually("the first gets everything", || engine.backup_status().shipped_lsn >= lsn);
    two.slow_snapshots.store(0, Ordering::SeqCst);
    let sent = snapshots(&engine);
    engine.checkpoint().unwrap();
    engine.backup_to(1, false).unwrap();
    assert_eq!(snapshots(&engine), sent, "segments, no other snapshot");
    let status = engine.backup_status();
    assert!(status.last_ok && status.last_at.is_some(), "{status:?}");
    assert!(status.destinations[1].shipped_lsn >= lsn, "{status:?}");
    let gone = Arc::downgrade(&engine);
    drop((db, engine));
    closed(gone);
    let (_, events, rows, _) = restored_from(TestPlace::reading(&b), "long-snapshot-from-two").await;
    assert_eq!(rows, n);
    assert!(!events.iter().any(|e| e.message.contains("the log holds commit")), "{events:?}");
    for d in [dir, a, b] {
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// A round to the second that hangs while much is written: the log kept
/// for it never takes more than the page store itself (four times
/// `checkpoint_wal_bytes` at least), so the disk does not fill. Once it
/// answers again, it gets a whole snapshot, and has every commit.
#[tokio::test]
async fn a_second_that_hangs_does_not_fill_the_disk() {
    let dir = temp_dir("hang-disk-data");
    let (a, b) = (temp_dir("hang-disk-one"), temp_dir("hang-disk-two"));
    let two = TestPlace::new(&b);
    let options = EngineOptions {
        checkpoint_every: Duration::from_millis(50),
        checkpoint_wal_bytes: 64 << 10,
        ..backed_up_to(mirror(TestPlace::new(&a), two.clone()), 1)
    };
    let db = Embedded::open_with(&dir, options).unwrap();
    let engine = db.engine().clone();
    first_round(&engine, 2);
    db.sql("CREATE TABLE notes (id BIGINT PRIMARY KEY, body TEXT)", &[]).unwrap();
    db.sql("INSERT INTO notes (id, body) VALUES (1, 'first')", &[]).unwrap();
    engine.backup_now(false).unwrap();
    two.set_hang(true);
    db.sql("UPDATE notes SET body = 'held' WHERE id = 1", &[]).unwrap();
    two.wait_held();
    // About 2 MB of log, on a page store of a few hundred KB.
    for n in 0..100 {
        let body = format!("{n:04}{}", "x".repeat(20_000));
        db.sql("UPDATE notes SET body = ? WHERE id = 1", &[serde_json::json!(body)]).unwrap();
    }
    let lsn = engine.stats().lsn;
    eventually("the first gets everything", || engine.backup_status().shipped_lsn >= lsn);
    engine.checkpoint().unwrap();
    engine.checkpoint().unwrap();
    let stats = engine.stats();
    assert!(stats.wal_bytes < stats.file_bytes.max(256 << 10) + (256 << 10), "{stats:?}");
    // It answers again: the round that hung cannot go on, the next one sends a whole
    // snapshot, and then it has everything.
    two.set_hang(false);
    eventually("the round that hung ends", || engine.backup_status().destinations[1].error.is_some());
    let e = engine.backup_status().destinations[1].error.clone().unwrap();
    assert!(e.contains("is no longer on this disk"), "{e}");
    engine.backup_to(1, false).unwrap();
    let s = engine.backup_status();
    assert!(s.last_ok && s.destinations[1].shipped_lsn >= lsn && s.destinations[1].error.is_none(), "{s:?}");
    let gone = Arc::downgrade(&engine);
    drop((db, engine));
    closed(gone);
    let restored = temp_dir("hang-disk-restored");
    let db = Embedded::open_with(&restored, backed_up_to(TestPlace::reading(&b), 1)).unwrap();
    let rows = db.sql("SELECT body FROM notes", &[]).unwrap();
    assert!(format!("{rows:?}").contains("0099xxxx"), "{rows:?}");
    let gone = Arc::downgrade(db.engine());
    drop(db);
    closed(gone);
    for d in [dir, restored, a, b] {
        let _ = std::fs::remove_dir_all(&d);
    }
}
