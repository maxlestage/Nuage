//! Timestamps, soft deletes, hooks, `first_or_create`/`update_or_create`,
//! chunks, cursor pagination, conditional queries and the query log.

use std::sync::Mutex;
use std::time::Duration;

use memoire_orm::lifecycle::Hooks;
use memoire_orm::log::Logged;
use memoire_orm::prelude::*;
use memoire_orm::{Database, Select, Transaction};

/// What the hooks of [`Doc`] saw, in order.
static HOOK_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn hook_log() -> Vec<String> {
    std::mem::take(&mut *HOOK_LOG.lock().unwrap())
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "lc_docs", timestamps, soft_deletes, hooks)]
pub struct Doc {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(unique)]
    pub title: String,
    pub slug: String,
    pub version: i32,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub deleted_at: Option<i64>,
}

impl Hooks for Doc {
    fn before_save(&mut self) -> Result<()> {
        if self.title.trim().is_empty() {
            return Err(Error::Validation("a doc needs a title".into()));
        }
        self.slug = self.title.to_lowercase().replace(' ', "-");
        Ok(())
    }

    fn before_insert(&mut self) -> Result<()> {
        self.version = 1;
        Ok(())
    }

    fn after_insert(&self) {
        HOOK_LOG.lock().unwrap().push(format!("inserted {}", self.id));
    }

    fn before_update(&self) -> Result<()> {
        if self.version < 1 { Err(Error::Validation("version must be positive".into())) } else { Ok(()) }
    }

    fn after_update(&self) {
        HOOK_LOG.lock().unwrap().push(format!("updated {}", self.id));
    }

    fn before_delete(&self) -> Result<()> {
        if self.title == "locked" { Err(Error::Validation("locked".into())) } else { Ok(()) }
    }

    fn after_delete(&self) {
        HOOK_LOG.lock().unwrap().push(format!("deleted {}", self.id));
    }
}

/// A child of [`Doc`], soft deletable too, with timestamps declared field by field.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "lc_comments")]
pub struct Comment {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(references = "lc_docs(id)", index)]
    pub doc_id: i64,
    pub body: String,
    #[memoire(created_at)]
    pub written: String,
    #[memoire(updated_at)]
    pub edited: Option<String>,
    #[memoire(soft_delete)]
    pub removed: Option<f64>,
}

/// No lifecycle at all: the behaviour must not change.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "lc_items")]
pub struct Item {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    pub name: String,
    pub rank: i32,
}

fn doc(title: &str) -> Doc {
    Doc {
        id: 0,
        title: title.into(),
        slug: String::new(),
        version: 0,
        created_at: None,
        updated_at: None,
        deleted_at: None,
    }
}

fn comment(doc_id: i64, body: &str) -> Comment {
    Comment { id: 0, doc_id, body: body.into(), written: String::new(), edited: None, removed: None }
}

fn tick() {
    std::thread::sleep(Duration::from_millis(3));
}

async fn reset<E: Executor>(db: &E) -> Result<()> {
    db.drop_table_if_exists::<Comment>().await?;
    db.drop_table_if_exists::<Doc>().await?;
    db.drop_table_if_exists::<Item>().await?;
    db.create_table::<Doc>().await?;
    db.create_table::<Comment>().await?;
    db.create_table::<Item>().await?;
    Ok(())
}

pub async fn run<D: Database>(db: &D) -> Result<()> {
    reset(db).await?;
    timestamps_and_hooks(db).await?;
    reset(db).await?;
    soft_deletes(db).await?;
    reset(db).await?;
    find_or_create(db).await?;
    reset(db).await?;
    chunks_and_cursors(db).await?;
    reset(db).await?;
    conditional_queries(db).await?;
    reset(db).await?;
    query_log(db).await?;
    reset(db).await?;
    Ok(())
}

async fn timestamps_and_hooks<E: Executor>(db: &E) -> Result<()> {
    hook_log();
    let mut a = doc("Hello World");
    a.insert(db).await?;
    assert_eq!((a.slug.as_str(), a.version), ("hello-world", 1), "before_save and before_insert ran");
    let created = a.created_at.expect("created_at is filled");
    assert!(created > 1_700_000_000_000, "milliseconds since the epoch: {created}");
    assert_eq!(a.updated_at, Some(created));
    assert_eq!(Doc::get(db, a.id).await?, a);
    assert_eq!(hook_log(), [format!("inserted {}", a.id)]);

    // A hook rejecting the row: nothing is written.
    let mut empty = doc("  ");
    assert!(empty.insert(db).await.unwrap_err().is_validation());
    assert_eq!(Doc::count(db).await?, 1);
    assert!(hook_log().is_empty());

    // `update` stamps `updated_at` in the database; `created_at` stays.
    tick();
    a.title = "Hello Rust".into();
    a.update(db).await?;
    let stored = Doc::get(db, a.id).await?;
    assert_eq!(stored.created_at, Some(created));
    assert!(stored.updated_at.unwrap() > created);
    assert_eq!(stored.slug, "hello-world", "update does not run before_save");
    assert_eq!(hook_log(), [format!("updated {}", a.id)]);

    // `save` runs before_save and writes `updated_at` back.
    tick();
    a.save(db).await?;
    assert_eq!(a.slug, "hello-rust");
    assert!(a.updated_at > stored.updated_at);
    assert_eq!(Doc::get(db, a.id).await?, a);
    assert_eq!(hook_log(), [format!("updated {}", a.id)]);

    // `save` of a new row inserts it.
    let mut b = doc("Second");
    b.save(db).await?;
    assert!(b.id > a.id && b.created_at.is_some());
    assert_eq!(hook_log(), [format!("inserted {}", b.id)]);

    // A hook rejecting an update.
    b.version = 0;
    assert!(b.update(db).await.unwrap_err().is_validation());
    assert!(b.update_only(db, Doc::VERSION).await.unwrap_err().is_validation());
    b.version = 2;

    // `update_only` adds `updated_at`; `touch` sets only it.
    tick();
    let before = Doc::get(db, b.id).await?.updated_at;
    b.update_only(db, Doc::VERSION).await?;
    let after = Doc::get(db, b.id).await?;
    assert_eq!(after.version, 2);
    assert!(after.updated_at > before);
    tick();
    assert!(b.touch(db).await?);
    assert!(b.updated_at > after.updated_at);
    assert_eq!(Doc::get(db, b.id).await?.updated_at, b.updated_at);
    hook_log();

    // Mass updates stamp `updated_at` too, unless it is set explicitly.
    tick();
    let n = Doc::update_many().increment(Doc::VERSION, 1).execute(db).await?;
    assert_eq!(n, 2);
    let docs = Doc::query().order_by(Doc::ID).all(db).await?;
    assert!(docs.iter().all(|d| d.updated_at > after.updated_at));
    Doc::update_many().set(Doc::UPDATED_AT, Some(5)).execute(db).await?;
    assert!(Doc::all(db).await?.iter().all(|d| d.updated_at == Some(5)));

    // `insert_many` and `upsert` fill unset timestamps; an explicit value is kept.
    let mut c = doc("third");
    c.created_at = Some(42);
    Doc::insert_many(db, &[c, doc("fourth")]).await?;
    let third = Doc::query().filter(Doc::TITLE.eq("third")).one(db).await?;
    let fourth = Doc::query().filter(Doc::TITLE.eq("fourth")).one(db).await?;
    assert_eq!(third.created_at, Some(42));
    assert!(third.updated_at.unwrap() > 42);
    assert!(fourth.created_at.is_some() && fourth.created_at == fourth.updated_at);
    assert!(hook_log().is_empty(), "no hook for mass inserts");
    let mut up = fourth.clone();
    up.version = 9;
    up.updated_at = None;
    tick();
    up.upsert(db).await?;
    let stored = Doc::get(db, up.id).await?;
    assert_eq!((stored.version, stored.created_at), (9, fourth.created_at));
    assert!(stored.updated_at > fourth.updated_at);

    // Timestamps in text columns are RFC 3339, and sort like the time.
    let mut first = comment(a.id, "first");
    first.insert(db).await?;
    assert_eq!(first.written.len(), "2026-10-15T08:30:00.123Z".len());
    assert!(first.written.ends_with('Z') && first.written.as_bytes()[10] == b'T');
    assert_eq!(first.edited.as_deref(), Some(first.written.as_str()));
    tick();
    first.body = "edited".into();
    first.save(db).await?;
    assert!(first.edited.as_deref().unwrap() > first.written.as_str());
    assert_eq!(Comment::get(db, first.id).await?, first);

    // A model without lifecycle behaves as before.
    let mut item = Item { id: 0, name: "plain".into(), rank: 1 };
    item.insert(db).await?;
    assert!(!item.touch(db).await?);
    assert!(!item.trashed());
    assert!(item.delete(db).await?);
    assert_eq!(Item::with_trashed().count(db).await?, 0);
    assert!(matches!(item.restore(db).await, Err(Error::Unsupported(_))));
    Ok(())
}

async fn soft_deletes<E: Executor>(db: &E) -> Result<()> {
    let mut docs = Vec::new();
    for title in ["alpha", "beta", "gamma", "delta"] {
        let mut d = doc(title);
        d.insert(db).await?;
        docs.push(d);
    }
    hook_log();

    // `delete` stamps `deleted_at`, and the row disappears from queries.
    tick();
    assert!(docs[1].delete(db).await?);
    assert!(!docs[1].delete(db).await?, "already deleted");
    assert_eq!(hook_log(), [format!("deleted {}", docs[1].id)]);
    assert_eq!(Doc::count(db).await?, 3);
    assert_eq!(Doc::find(db, docs[1].id).await?, None);
    assert!(Doc::get(db, docs[1].id).await.unwrap_err().is_not_found());
    assert_eq!(Doc::find_many(db, [docs[0].id, docs[1].id]).await?.len(), 1);
    assert!(!Doc::query().filter(Doc::TITLE.eq("beta")).exists(db).await?);
    assert_eq!(Doc::query().paginate(db, 1, 10).await?.total, 3);

    // ... but is still there.
    assert_eq!(Doc::with_trashed().count(db).await?, 4);
    let trashed = Doc::only_trashed().all(db).await?;
    assert_eq!(trashed.len(), 1);
    let beta = trashed[0].clone();
    assert!(beta.trashed() && !docs[0].trashed());
    assert!(beta.deleted_at.unwrap() > beta.created_at.unwrap());
    assert_eq!(beta.updated_at, beta.deleted_at);
    assert_eq!(Doc::query().with_trashed().without_trashed().count(db).await?, 3);

    // The scope applies to the whole condition, `or_filter` included.
    let titles: Vec<String> = Doc::query()
        .filter(Doc::TITLE.eq("alpha"))
        .or_filter(Doc::TITLE.eq("beta"))
        .order_by(Doc::ID)
        .columns(Doc::TITLE)
        .all(db)
        .await?;
    assert_eq!(titles, ["alpha"]);
    let both = Doc::with_trashed().filter(Doc::TITLE.eq("alpha")).or_filter(Doc::TITLE.eq("beta")).count(db).await?;
    assert_eq!(both, 2);

    // A hook can refuse a delete.
    let mut locked = doc("locked");
    locked.insert(db).await?;
    assert!(locked.delete(db).await.unwrap_err().is_validation());
    assert!(locked.force_delete(db).await.unwrap_err().is_validation());
    assert_eq!(Doc::count(db).await?, 4);
    locked.title = "unlocked".into();
    locked.update(db).await?;

    // `restore` brings the row back, `force_delete` removes it for good.
    let mut beta = beta;
    assert!(beta.restore(db).await?);
    assert!(!beta.trashed());
    assert!(!beta.restore(db).await?, "not deleted any more");
    assert_eq!(Doc::get(db, beta.id).await?, beta);
    hook_log();
    assert!(beta.force_delete(db).await?);
    assert_eq!(hook_log(), [format!("deleted {}", beta.id)]);
    assert_eq!(Doc::with_trashed().count(db).await?, 4);

    // `delete_by_key` and `delete_many` are soft too; `force()` is not.
    assert!(Doc::delete_by_key(db, docs[0].id).await?);
    assert_eq!(Doc::delete_many().filter(Doc::TITLE.eq("gamma")).execute(db).await?, 1);
    assert_eq!(Doc::delete_many().filter(Doc::TITLE.eq("gamma")).execute(db).await?, 0);
    assert_eq!(Doc::count(db).await?, 2);
    assert_eq!(Doc::with_trashed().count(db).await?, 4);
    assert_eq!(Doc::delete_many().execute(db).await?, 2);
    assert_eq!(Doc::count(db).await?, 0);
    assert_eq!(Doc::delete_many().force().filter(Doc::TITLE.eq("alpha")).execute(db).await?, 1);
    assert_eq!(Doc::with_trashed().count(db).await?, 3);

    // Relations leave soft-deleted rows out.
    let mut parent = doc("parent");
    parent.insert(db).await?;
    let mut kept = comment(parent.id, "kept");
    kept.insert(db).await?;
    let mut gone = comment(parent.id, "gone");
    gone.insert(db).await?;
    assert!(gone.delete(db).await?);
    let children: Vec<Vec<Comment>> = [parent.clone()].load_children(db).await?;
    assert_eq!(children[0], [kept.clone()]);
    assert_eq!(Comment::belonging_to(&parent).count(db).await?, 1);
    assert_eq!(Comment::belonging_to(&parent).with_trashed().count(db).await?, 2);
    let removed = Comment::only_trashed().one(db).await?;
    assert!(removed.removed.unwrap() > 1_700_000_000.0, "seconds in an f64 column");
    parent.delete(db).await?;
    let parents: Vec<Option<Doc>> = [kept].load_parent(db).await?;
    assert_eq!(parents, [None]);
    Ok(())
}

async fn find_or_create<E: Executor>(db: &E) -> Result<()> {
    let by_title = |t: &str| Doc::query().filter(Doc::TITLE.eq(t));
    let made = by_title("faq").first_or_create(db, || doc("faq")).await?;
    assert!(made.id > 0 && made.created_at.is_some(), "inserted through insert(), with timestamps");
    let again = by_title("faq").first_or_create(db, || unreachable!("the row exists")).await?;
    assert_eq!(again, made);
    assert_eq!(Doc::count(db).await?, 1);

    // The query matched nothing but the insert hits the unique title: the
    // existing row is returned (another writer created it in the meantime).
    let raced = Doc::query()
        .filter(Doc::VERSION.eq(100))
        .or_filter(Doc::TITLE.eq("faq"))
        .filter(Doc::VERSION.eq(100))
        .first_or_create(db, || doc("faq"))
        .await;
    assert!(raced.unwrap_err().is_unique_violation(), "the query still matches nothing");

    // `update_or_create`: updates the match, or creates it.
    let updated = by_title("faq").update_or_create(db, || unreachable!(), |d| d.version = 7).await?;
    assert_eq!((updated.id, updated.version), (made.id, 7));
    assert_eq!(Doc::get(db, made.id).await?.version, 7);
    let created = by_title("new").update_or_create(db, || doc("new"), |d| d.slug = "ignored".into()).await?;
    assert_eq!((created.version, created.slug.as_str()), (1, "new"), "hooks ran after the change");
    assert_eq!(Doc::count(db).await?, 2);

    // A model without lifecycle.
    let mut item = Item { id: 0, name: "x".into(), rank: 1 };
    item.insert(db).await?;
    let found = Item::query().filter(Item::NAME.eq("x")).first_or_create(db, || unreachable!()).await?;
    assert_eq!(found, item);
    Ok(())
}

async fn chunks_and_cursors<E: Executor>(db: &E) -> Result<()> {
    let items: Vec<Item> = (1..=23).map(|n| Item { id: 0, name: format!("item {n}"), rank: n % 3 }).collect();
    Item::insert_many(db, &items).await?;

    // Chunks by primary key, the query's filters kept, its order replaced.
    let mut chunks = Item::query().filter(Item::RANK.ne(0)).order_by(Item::NAME.desc()).limit(1).chunks(5);
    let mut seen = Vec::new();
    let mut sizes = Vec::new();
    while let Some(rows) = chunks.next(db).await? {
        sizes.push(rows.len());
        seen.extend(rows.into_iter().map(|i| i.id));
    }
    assert!(chunks.next(db).await?.is_none());
    let expected: Vec<i64> = (1..=23).filter(|n| n % 3 != 0).collect();
    assert_eq!(seen, expected);
    assert_eq!(sizes, [5, 5, 5, 1]);

    // Rows deleted while reading are not a problem, nor are exact multiples.
    let mut chunks = Item::query().filter(Item::ID.le(20)).chunks(10);
    let first = chunks.next(db).await?.unwrap();
    Item::delete_many().filter(Item::ID.eq(15)).execute(db).await?;
    let second = chunks.next(db).await?.unwrap();
    assert_eq!((first.len(), second.len()), (10, 9));
    assert!(chunks.next(db).await?.is_none());

    let mut total_rank = 0;
    let n = Item::query()
        .chunks(4)
        .for_each(db, |rows| {
            total_rank += rows.iter().map(|i| i.rank).sum::<i32>();
            Ok(())
        })
        .await?;
    assert_eq!(n, 22);
    assert_eq!(total_rank, (1..=23).filter(|&n| n != 15).map(|n| n % 3).sum::<i32>());
    let err = Item::query().chunks(4).for_each(db, |_| Err(Error::Validation("stop".into()))).await;
    assert!(err.unwrap_err().is_validation());

    // Cursor pagination walks every row once, in key order.
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let page = Item::query().filter(Item::RANK.eq(1)).cursor_paginate(db, 3, cursor).await?;
        assert_eq!(page.per_page, 3);
        pages.push(page.items.iter().map(|i| i.id).collect::<Vec<_>>());
        if !page.has_more() {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(pages, [vec![1, 4, 7], vec![10, 13, 16], vec![19, 22]]);
    let exact = Item::query().filter(Item::ID.le(6)).cursor_paginate(db, 3, Some(Value::I64(3))).await?;
    assert_eq!(exact.items.len(), 3);
    assert!(!exact.has_more(), "no empty last page");

    // With soft deletes, deleted rows are skipped.
    for t in ["a", "b", "c", "d"] {
        doc(t).insert(db).await?;
    }
    Doc::delete_many().filter(Doc::TITLE.eq("b")).execute(db).await?;
    let page = Doc::query().cursor_paginate(db, 10, None).await?;
    assert_eq!(page.items.iter().map(|d| d.title.as_str()).collect::<Vec<_>>(), ["a", "c", "d"]);
    let mut chunks = Doc::with_trashed().chunks(10);
    assert_eq!(chunks.next(db).await?.unwrap().len(), 4);
    Ok(())
}

fn ranked(q: Select<Item>) -> Select<Item> {
    q.filter(Item::RANK.gt(0)).order_by(Item::ID)
}

async fn conditional_queries<E: Executor>(db: &E) -> Result<()> {
    for n in 1..=6 {
        Item { id: 0, name: format!("item {n}"), rank: n % 3 }.insert(db).await?;
    }
    let search = |rank: Option<i32>, name: Option<&str>, newest_first: bool| {
        Item::query()
            .when_some(rank, |q, r| q.filter(Item::RANK.eq(r)))
            .when_some(name, |q, n| q.filter(Item::NAME.eq(n)))
            .when(newest_first, |q| q.order_by(Item::ID.desc()))
            .when(!newest_first, |q| q.order_by(Item::ID.asc()))
            .columns(Item::ID)
    };
    assert_eq!(search(None, None, false).all(db).await?, [1, 2, 3, 4, 5, 6]);
    assert_eq!(search(Some(1), None, true).all(db).await?, [4, 1]);
    assert_eq!(search(Some(2), Some("item 5"), false).all(db).await?, [5]);
    let ids: Vec<i64> = Item::query().apply(ranked).columns(Item::ID).all(db).await?;
    assert_eq!(ids, [1, 2, 4, 5]);
    Ok(())
}

async fn query_log<D: Database>(inner: &D) -> Result<()> {
    let db = Logged::new(inner);
    let mut parent = doc("logged");
    parent.insert(&db).await?;
    for body in ["one", "two"] {
        comment(parent.id, body).insert(&db).await?;
    }
    assert_eq!(db.query_count(), 3);
    let insert = &db.queries()[0];
    assert!(insert.sql.starts_with("INSERT INTO"), "{}", insert.sql);
    assert_eq!(insert.rows, Some(1));
    assert!(!insert.in_transaction && insert.error.is_none());
    assert!(insert.params.contains(&Value::Text("logged".into())));
    #[cfg(not(target_arch = "wasm32"))]
    assert!(insert.duration.is_some());
    db.clear();

    // Spotting N+1 queries: eager loading is one query per level.
    let docs = Doc::all(&db).await?;
    let _: Vec<Vec<Comment>> = docs.load_children(&db).await?;
    let log = db.take_queries();
    assert_eq!(log.len(), 2);
    assert!(log[1].sql.contains(r#""lc_comments"."doc_id" IN ("#), "{}", log[1].sql);
    assert_eq!(log[1].rows, Some(2));
    assert_eq!(db.query_count(), 0);

    // Failures are logged too, with their error.
    let err = doc("logged").insert(&db).await.unwrap_err();
    assert!(err.is_unique_violation());
    let failed = db.take_queries().pop().unwrap();
    assert_eq!(failed.rows, None);
    assert!(failed.error.is_some());
    assert!(failed.to_string().contains("failed"));

    // Transactions.
    let tx = db.begin().await?;
    Item { id: 0, name: "in tx".into(), rank: 0 }.insert(&tx).await?;
    tx.commit().await?;
    let log = db.take_queries();
    let sql: Vec<&str> = log.iter().map(|q| if q.sql.starts_with("INSERT") { "INSERT" } else { &q.sql }).collect();
    assert_eq!(sql, ["BEGIN", "INSERT", "COMMIT"]);
    assert!(log.iter().all(|q| q.in_transaction));

    // Listeners, with or without recording; a bounded log.
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&seen);
    db.listen(move |q| sink.lock().unwrap().push(q.to_string()));
    db.set_recording(false);
    Item::count(&db).await?;
    assert_eq!(db.query_count(), 0);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(seen.lock().unwrap()[0].starts_with("SELECT COUNT(*)"), "{:?}", seen.lock().unwrap());
    db.set_recording(true);

    let bounded = Logged::with_limit(inner, 2);
    for _ in 0..5 {
        Item::count(&bounded).await?;
    }
    assert_eq!(bounded.query_count(), 2);
    Item::insert_many(&bounded, &[Item { id: 0, name: "bulk".into(), rank: 0 }]).await?;
    assert!(bounded.queries()[1].sql.starts_with("INSERT INTO"));
    Ok(())
}
