//! Subqueries, relation filters and counts, eager loading, polymorphic
//! relations, pivot rows, enum casts, optimistic locking, factories and
//! seeders, the identity map and keyset pagination on any column.

use std::sync::Arc;

use memoire_orm::factory::{Factory, Seeder, seed};
use memoire_orm::identity::IdentityMap;
use memoire_orm::prelude::*;
use memoire_orm::{Cursor, Database, FromValue};

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Reader,
    #[memoire(rename = "boss")]
    SuperAdmin,
}

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[memoire(integer)]
pub enum Level {
    Low = 1,
    High = 10,
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "adv_authors")]
pub struct Author {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(unique)]
    pub name: String,
    pub role: Role,
    pub level: Option<Level>,
}

impl Factory for Author {
    fn definition(n: u64) -> Self {
        Author { id: 0, name: format!("author {n}"), role: Role::Reader, level: None }
    }
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "adv_articles")]
pub struct Article {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(references = "adv_authors(id)", index)]
    pub author_id: Option<i64>,
    pub title: String,
    pub published: bool,
    pub score: i32,
}

impl Factory for Article {
    fn definition(n: u64) -> Self {
        Article { id: 0, author_id: None, title: format!("article {n}"), published: true, score: 0 }
    }
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "adv_labels")]
pub struct Label {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    pub name: String,
}

/// The junction of articles and labels, with a pivot column.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "adv_article_labels")]
pub struct ArticleLabel {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(references = "adv_articles(id)")]
    pub article_id: i64,
    #[memoire(references = "adv_labels(id)")]
    pub label_id: i64,
    pub main: bool,
}

/// Polymorphic: a note on an article or on an author.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "adv_notes", morph = "notable")]
pub struct Note {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    pub notable_type: String,
    pub notable_id: i64,
    pub body: String,
}

/// Optimistic locking.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "adv_accounts")]
pub struct Account {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    pub balance: i64,
    #[memoire(version)]
    pub version: i32,
}

/// Optimistic locking with soft deletes and a key of its own.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "adv_settings", soft_deletes)]
pub struct Setting {
    #[memoire(primary_key)]
    pub key: String,
    pub value: String,
    #[memoire(version)]
    pub lock_version: i64,
    pub deleted_at: Option<i64>,
}

struct Authors;
impl Seeder for Authors {
    async fn run<E: Executor>(&self, db: &E) -> Result<()> {
        Author::factory().count(2).create(db).await?;
        Ok(())
    }
}

struct Boss;
impl Seeder for Boss {
    async fn run<E: Executor>(&self, db: &E) -> Result<()> {
        Author::factory().state(|a| a.role = Role::SuperAdmin).create_one(db).await?;
        Ok(())
    }
}

struct Failing;
impl Seeder for Failing {
    async fn run<E: Executor>(&self, db: &E) -> Result<()> {
        Author::factory().create_one(db).await?;
        Err(Error::Validation("stop".into()))
    }
}

fn names<T>(rows: &[T], f: impl Fn(&T) -> &str) -> Vec<String> {
    rows.iter().map(|r| f(r).to_owned()).collect()
}

pub async fn run<D: Database>(db: &D) -> Result<()> {
    for _ in 0..2 {
        let _ = db.drop_table_if_exists::<ArticleLabel>().await;
        let _ = db.drop_table_if_exists::<Note>().await;
        let _ = db.drop_table_if_exists::<Label>().await;
        let _ = db.drop_table_if_exists::<Article>().await;
        let _ = db.drop_table_if_exists::<Author>().await;
        let _ = db.drop_table_if_exists::<Account>().await;
        let _ = db.drop_table_if_exists::<Setting>().await;
    }
    db.create_table::<Author>().await?;
    db.create_table::<Article>().await?;
    db.create_table::<Label>().await?;
    db.create_table::<ArticleLabel>().await?;
    db.create_table::<Note>().await?;
    db.create_table::<Account>().await?;
    db.create_table::<Setting>().await?;

    factories_and_seeders(db).await?;
    enums(db).await?;
    relations(db).await?;
    subqueries(db).await?;
    morphs_and_pivots(db).await?;
    optimistic_locking(db).await?;
    identity_map(db).await?;
    keyset(db).await?;
    Ok(())
}

async fn factories_and_seeders<D: Database>(db: &D) -> Result<()> {
    Author::reset_sequence();
    let made = Author::factory().count(3).sequence(|a, i| a.level = (i == 1).then_some(Level::High)).make()?;
    assert_eq!(names(&made, |a| &a.name), ["author 1", "author 2", "author 3"]);
    assert_eq!(made[1].level, Some(Level::High));
    assert_eq!(Author::count(db).await?, 0, "make stores nothing");

    seed(db, &(Authors, Boss)).await?;
    assert_eq!(Author::count(db).await?, 3);
    assert_eq!(Author::query().filter(Author::ROLE.eq(Role::SuperAdmin)).count(db).await?, 1);
    // A failing seeder writes nothing.
    assert!(seed(db, &Failing).await.unwrap_err().is_validation());
    assert_eq!(Author::count(db).await?, 3);

    let boss = Author::query().filter(Author::ROLE.eq(Role::SuperAdmin)).one(db).await?;
    let posts = Article::factory().count(2).for_parent(&boss).create(db).await?;
    assert!(posts.iter().all(|p| p.author_id == Some(boss.id) && p.id > 0));
    assert!(Label::factory_missing_link().is_err());
    Ok(())
}

impl Factory for Label {
    fn definition(n: u64) -> Self {
        Label { id: 0, name: format!("label {n}") }
    }
}

impl Label {
    /// A label has no foreign key to an author.
    fn factory_missing_link() -> Result<Vec<Label>> {
        Label::factory().for_parent(&Author::definition(0)).make()
    }
}

async fn enums<D: Database>(db: &D) -> Result<()> {
    let mut a = Author { id: 0, name: "enum".into(), role: Role::SuperAdmin, level: Some(Level::Low) };
    a.insert(db).await?;
    let back = Author::get(db, a.id).await?;
    assert_eq!(back, a);
    let (role, level): (Value, Value) = Author::query()
        .filter(Author::ID.eq(a.id))
        .select_exprs([Author::ROLE.expr(), Author::LEVEL.expr()])
        .one(db)
        .await?;
    assert_eq!(role, Value::Text("boss".into()));
    assert!(matches!(level, Value::I32(1) | Value::I64(1)), "{level:?}");
    assert_eq!(Author::query().filter(Author::LEVEL.eq(Level::Low)).count(db).await?, 1);
    assert!(Role::from_value(Value::Text("nope".into())).unwrap_err().to_string().contains("reader, boss"));
    a.delete(db).await?;
    Ok(())
}

async fn relations<D: Database>(db: &D) -> Result<()> {
    // Authors: author 1 (no article), author 2 (no article), boss (2 articles).
    let boss = Author::query().filter(Author::ROLE.eq(Role::SuperAdmin)).one(db).await?;
    let mut draft = Article::factory().for_parent(&boss).state(|a| a.published = false).make_one()?;
    draft.insert(db).await?;
    Article::factory().state(|a| a.title = "orphan".into()).create_one(db).await?;

    let with_articles = Author::query().has::<Article>().all(db).await?;
    assert_eq!(names(&with_articles, |a| &a.name), std::slice::from_ref(&boss.name));
    let without = Author::query().doesnt_have::<Article>().order_by(Author::ID).all(db).await?;
    assert_eq!(without.len(), 2);
    let drafting = Author::query().where_has::<Article>(|q| q.filter(Article::PUBLISHED.eq(false))).count(db).await?;
    assert_eq!(drafting, 1);
    let all_published = Author::query().where_doesnt_have::<Article>(|q| q.filter(Article::PUBLISHED.eq(false)));
    assert_eq!(all_published.count(db).await?, 2);
    assert_eq!(Author::query().has_at_least::<Article>(3).count(db).await?, 1);
    assert_eq!(Author::query().has_at_least::<Article>(4).count(db).await?, 0);

    // The other way: articles that belong to an author, or not.
    assert_eq!(Article::query().has::<Author>().count(db).await?, 3);
    let orphans = Article::query().doesnt_have::<Author>().all(db).await?;
    assert_eq!(names(&orphans, |a| &a.title), ["orphan"]);
    let by_boss = Article::query().where_has::<Author>(|q| q.filter(Author::ROLE.eq(Role::SuperAdmin)));
    assert_eq!(by_boss.count(db).await?, 3);

    // Counts and eager loading.
    let authors = Author::query().order_by(Author::ID).all(db).await?;
    assert_eq!(authors.load_count::<Article>(db).await?, [0, 0, 3]);
    let drafts = authors.load_count_with(db, Article::query().filter(Article::PUBLISHED.eq(false))).await?;
    assert_eq!(drafts, [0, 0, 1]);
    let counted = Author::query().order_by(Author::ID.desc()).all_with_count::<Article>(db).await?;
    assert_eq!((counted[0].0.id, counted[0].1), (boss.id, 3));
    let loaded = Author::query().filter(Author::ID.eq(boss.id)).all_with::<Article>(db).await?;
    assert_eq!(loaded[0].1.len(), 3);
    let parents = Article::query().order_by(Article::ID).all_with_parent::<Author>(db).await?;
    assert_eq!(parents.iter().filter(|(_, p)| p.is_some()).count(), 3);
    Ok(())
}

async fn subqueries<D: Database>(db: &D) -> Result<()> {
    let published_authors = Article::query().filter(Article::PUBLISHED.eq(true)).columns(Article::AUTHOR_ID);
    let n = Author::query().filter(Author::ID.in_query(published_authors.clone())).count(db).await?;
    assert_eq!(n, 1);
    let not_null = published_authors.filter(Article::AUTHOR_ID.is_not_null());
    let n = Author::query().filter(Author::ID.not_in_query(not_null)).count(db).await?;
    assert_eq!(n, 2);
    // Nested, and in an UPDATE and a DELETE.
    let bosses = Author::query().filter(Author::ROLE.eq(Role::SuperAdmin)).columns(Author::ID);
    let changed = Article::update_many()
        .set(Article::SCORE, 7)
        .filter(Article::AUTHOR_ID.in_query(bosses.clone()))
        .execute(db)
        .await?;
    assert_eq!(changed, 3);
    let tx = db.begin().await?;
    let removed = Article::delete_many().filter(Article::AUTHOR_ID.in_query(bosses)).execute(&tx).await?;
    assert_eq!(removed, 3);
    tx.rollback().await?;
    assert_eq!(Article::query().filter(Article::SCORE.eq(7)).count(db).await?, 3);
    Ok(())
}

async fn morphs_and_pivots<D: Database>(db: &D) -> Result<()> {
    let articles = Article::query().filter(Article::AUTHOR_ID.is_not_null()).order_by(Article::ID).all(db).await?;
    let boss = Author::query().filter(Author::ROLE.eq(Role::SuperAdmin)).one(db).await?;
    let mut n1 = Note { id: 0, notable_type: String::new(), notable_id: 0, body: "on article".into() };
    n1.set_morph_parent(&articles[0])?;
    n1.insert(db).await?;
    let mut n2 = Note { id: 0, notable_type: String::new(), notable_id: 0, body: "on author".into() };
    n2.set_morph_parent(&boss)?;
    n2.insert(db).await?;
    assert_eq!(n2.notable_type, "adv_authors");

    assert_eq!(Note::morphed_by(&articles[0]).all(db).await?, [n1.clone()]);
    assert_eq!(n1.morph_parent::<Article, _>(db).await?, Some(articles[0].clone()));
    assert_eq!(n1.morph_parent::<Author, _>(db).await?, None);
    let notes = articles.load_morph_many::<Note>(db).await?;
    assert_eq!((notes[0].len(), notes[1].len()), (1, 0));
    let all = Note::query().order_by(Note::ID).all(db).await?;
    let authors: Vec<Option<Author>> = all.load_morph_parent(db).await?;
    assert_eq!(authors, [None, Some(boss.clone())]);

    let mut rust = Label { id: 0, name: "rust".into() };
    rust.insert(db).await?;
    let mut orm = Label { id: 0, name: "orm".into() };
    orm.insert(db).await?;
    for (label, main) in [(rust.id, true), (orm.id, false)] {
        ArticleLabel { id: 0, article_id: articles[0].id, label_id: label, main }.insert(db).await?;
    }
    let pivots = articles.load_via_pivot::<ArticleLabel, Label>(db).await?;
    assert_eq!(pivots[0].len(), 2);
    assert_eq!((pivots[0][0].0.name.as_str(), pivots[0][0].1.main), ("rust", true));
    assert!(pivots[1].is_empty());
    let main = articles
        .load_via_pivot_with::<ArticleLabel, Label>(db, ArticleLabel::query().filter(ArticleLabel::MAIN.eq(true)))
        .await?;
    assert_eq!(main[0].len(), 1);
    let tagged = Article::query().where_has_via::<ArticleLabel, Label>(|q| q.filter(Label::NAME.eq("orm")));
    assert_eq!(tagged.all(db).await?, [articles[0].clone()]);
    Ok(())
}

async fn optimistic_locking<D: Database>(db: &D) -> Result<()> {
    let mut mine = Account { id: 0, balance: 100, version: 0 };
    mine.insert(db).await?;
    let mut theirs = Account::get(db, mine.id).await?;

    mine.balance = 50;
    mine.save(db).await?;
    assert_eq!(mine.version, 1);
    theirs.balance = 70;
    let err = theirs.save(db).await.unwrap_err();
    assert!(err.is_stale(), "{err:?}");
    assert!(theirs.update(db).await.unwrap_err().is_stale());
    assert!(theirs.update_only(db, Account::BALANCE).await.unwrap_err().is_stale());
    assert!(theirs.delete(db).await.unwrap_err().is_stale());
    theirs.reload(db).await?;
    assert_eq!((theirs.balance, theirs.version), (50, 1));

    mine.update(db).await?; // stale afterwards: `update` borrows `mine`
    assert!(mine.save(db).await.unwrap_err().is_stale());
    mine.reload(db).await?;
    assert_eq!(mine.version, 2);
    Account::update_many().set(Account::BALANCE, 0).execute(db).await?;
    assert!(mine.update_only(db, Account::BALANCE).await.unwrap_err().is_stale());
    mine.reload(db).await?;
    assert_eq!(mine.version, 3);
    assert!(mine.delete(db).await?);
    assert!(!mine.delete(db).await?);
    assert!(Account { id: 999, balance: 0, version: 0 }.update(db).await.unwrap_err().is_not_found());

    // A key of its own: `save` inserts, then updates; soft deletes check the version too.
    let mut s = Setting { key: "theme".into(), value: "dark".into(), lock_version: 0, deleted_at: None };
    s.save(db).await?;
    s.value = "light".into();
    s.save(db).await?;
    assert_eq!(s.lock_version, 1);
    let stale = Setting { lock_version: 0, ..s.clone() };
    assert!(stale.delete(db).await.unwrap_err().is_stale());
    assert!(s.delete(db).await?);
    assert_eq!(Setting::with_trashed().count(db).await?, 1);
    Ok(())
}

async fn identity_map<D: Database>(db: &D) -> Result<()> {
    let map = IdentityMap::new();
    let ids: Vec<i64> = Author::query().order_by(Author::ID).columns(Author::ID).all(db).await?;
    let a = map.find::<Author, _>(db, ids[0]).await?.expect("author");
    let b = map.find::<Author, _>(db, ids[0]).await?.expect("author");
    assert!(Arc::ptr_eq(&a, &b));
    let many = map.find_many::<Author, _, _>(db, [ids[1], ids[0], 424_242]).await?;
    assert_eq!(many.len(), 2);
    assert!(Arc::ptr_eq(&many[1], &a));
    let merged = map.merge(Author::query().order_by(Author::ID).all(db).await?);
    assert!(Arc::ptr_eq(&merged[0], &a));
    assert!(map.get::<Author>(ids[0]).is_some());
    assert!(map.forget::<Author>(ids[0]));
    assert!(map.get::<Author>(ids[0]).is_none());
    map.clear();
    assert!(map.is_empty());
    Ok(())
}

async fn keyset<D: Database>(db: &D) -> Result<()> {
    ArticleLabel::delete_many().execute(db).await?;
    Article::delete_many().execute(db).await?;
    for (i, score) in [10, 30, 20, 30, 5, 20].into_iter().enumerate() {
        Article { id: 0, author_id: None, title: format!("k{i}"), published: true, score }.insert(db).await?;
    }
    let mut seen = Vec::new();
    let mut after: Option<Cursor> = None;
    loop {
        let page = Article::query().cursor_paginate_by(db, Article::SCORE.desc(), 4, after).await?;
        seen.extend(page.items.iter().map(|a| a.score));
        if !page.has_more() {
            break;
        }
        after = page.next;
    }
    assert_eq!(seen, [30, 30, 20, 20, 10, 5]);
    let asc = Article::query().cursor_paginate_by(db, Article::SCORE, 3, None).await?;
    assert_eq!(asc.items.iter().map(|a| a.score).collect::<Vec<_>>(), [5, 10, 20]);
    let next = Article::query().cursor_paginate_by(db, Article::SCORE, 3, asc.next).await?;
    assert_eq!(next.items.iter().map(|a| a.score).collect::<Vec<_>>(), [20, 30, 30]);
    assert!(!next.has_more());
    Ok(())
}
