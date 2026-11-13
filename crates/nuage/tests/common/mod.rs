//! A backend-agnostic test suite: every backend must behave identically.
#![allow(dead_code)]

use memoire_orm::prelude::*;
use memoire_orm::{Database, Transaction};

pub mod advanced;
pub mod lifecycle;

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "users")]
pub struct User {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(unique)]
    pub email: String,
    #[memoire(index)]
    pub name: String,
    pub age: Option<i32>,
    pub active: bool,
    pub score: f64,
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "posts")]
pub struct Post {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(references = "users(id)", on_delete = "cascade", index)]
    pub user_id: i64,
    pub title: String,
    pub views: i32,
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "tags")]
pub struct Tag {
    #[memoire(primary_key)]
    pub slug: String,
    pub label: String,
}

/// Junction table of the many-to-many relation between posts and tags.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "post_tags")]
pub struct PostTag {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    #[memoire(references = "posts(id)", on_delete = "cascade", index)]
    pub post_id: i64,
    #[memoire(references = "tags(slug)", on_delete = "cascade", index)]
    pub tag_slug: String,
}

/// Indexes over several columns: one unique, one plain.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "memberships", unique(user_id, team), index(team, role))]
pub struct Membership {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    pub user_id: i64,
    pub team: Option<String>,
    pub role: String,
}

pub fn member(user_id: i64, team: Option<&str>, role: &str) -> Membership {
    Membership { id: 0, user_id, team: team.map(Into::into), role: role.into() }
}

/// Full-text search, with a word index on `body`.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "articles")]
pub struct Article {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    pub title: String,
    #[memoire(fulltext)]
    pub body: Option<String>,
}

#[derive(FromRow, Debug, PartialEq)]
pub struct UserStats {
    pub user_id: i64,
    pub posts: i64,
    pub total_views: Option<i64>,
}

pub fn user(email: &str, name: &str, age: Option<i32>, score: f64) -> User {
    User { id: 0, email: email.into(), name: name.into(), age, active: true, score }
}

async fn reset<E: Executor>(db: &E) -> Result<()> {
    db.drop_table_if_exists::<PostTag>().await?;
    db.drop_table_if_exists::<Post>().await?;
    db.drop_table_if_exists::<User>().await?;
    db.drop_table_if_exists::<Tag>().await?;
    db.drop_table_if_exists::<Membership>().await?;
    db.drop_table_if_exists::<Article>().await?;
    db.execute(memoire_orm::Query::DropTable { name: "_memoire_migrations", if_exists: true }).await?;
    db.create_table::<User>().await?;
    db.create_table::<Post>().await?;
    db.create_table::<Tag>().await?;
    db.create_table::<PostTag>().await?;
    db.create_table::<Membership>().await?;
    db.create_table::<Article>().await?;
    Ok(())
}

async fn seed<E: Executor>(db: &E) -> Result<Vec<User>> {
    let mut users = vec![
        user("alice@example.com", "Alice", Some(31), 1.5),
        user("bob@example.com", "Bob", Some(17), 2.5),
        user("carol@example.com", "Carol", None, 3.0),
        user("dave@example.com", "Dave", Some(45), 4.0),
        user("eve@example.com", "Eve", Some(22), 0.5),
    ];
    for u in &mut users {
        u.insert(db).await?;
    }
    Ok(users)
}

pub async fn run_suite<D: Database>(db: &D) -> Result<()> {
    #[cfg(all(feature = "chrono", feature = "uuid", feature = "json"))]
    typed::run(db).await?;
    reset(db).await?;
    crud(db).await?;
    reset(db).await?;
    queries(db).await?;
    reset(db).await?;
    updates_and_deletes(db).await?;
    reset(db).await?;
    joins_and_groups(db).await?;
    reset(db).await?;
    relations(db).await?;
    reset(db).await?;
    composite_indexes(db).await?;
    reset(db).await?;
    full_text(db).await?;
    reset(db).await?;
    events(db).await?;
    reset(db).await?;
    transactions(db).await?;
    reset(db).await?;
    transaction_helper(db).await?;
    reset(db).await?;
    push_schema(db).await?;
    reset(db).await?;
    bulk(db).await?;
    reset(db).await?;
    errors(db).await?;
    migrations(db).await?;
    Ok(())
}

async fn crud<E: Executor>(db: &E) -> Result<()> {
    let users = seed(db).await?;
    assert_eq!(users.iter().map(|u| u.id).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5], "generated keys");

    let alice = User::get(db, 1).await?;
    assert_eq!(alice, users[0]);
    assert_eq!(User::find(db, 99).await?, None);
    assert!(User::get(db, 99).await.unwrap_err().is_not_found());
    assert_eq!(User::count(db).await?, 5);

    let mut bob = User::get(db, 2).await?;
    bob.name = "Robert".into();
    bob.age = None;
    bob.update(db).await?;
    let reloaded = User::get(db, 2).await?;
    assert_eq!(reloaded.name, "Robert");
    assert_eq!(reloaded.age, None);

    let mut ghost = user("ghost@example.com", "Ghost", None, 0.0);
    ghost.id = 999;
    assert!(ghost.update(db).await.unwrap_err().is_not_found());

    assert!(bob.delete(db).await?);
    assert!(!bob.delete(db).await?);
    assert_eq!(User::count(db).await?, 4);
    assert!(User::delete_by_key(db, 3).await?);

    let mut eve = User::get(db, 5).await?;
    eve.score = 99.0;
    eve.update(db).await?;
    eve.score = 0.0;
    eve.reload(db).await?;
    assert_eq!(eve.score, 99.0);

    // Non auto-increment string key + upsert.
    let mut tag = Tag { slug: "rust".into(), label: "Rust".into() };
    tag.insert(db).await?;
    tag.label = "Rust 🦀".into();
    tag.upsert(db).await?;
    Tag { slug: "orm".into(), label: "ORM".into() }.upsert(db).await?;
    assert_eq!(Tag::get(db, "rust").await?.label, "Rust 🦀");
    assert_eq!(Tag::count(db).await?, 2);
    Ok(())
}

async fn queries<E: Executor>(db: &E) -> Result<()> {
    seed(db).await?;

    let names = |v: Vec<User>| v.into_iter().map(|u| u.name).collect::<Vec<_>>();

    let adults = User::query().filter(User::AGE.ge(18)).order_by(User::NAME.asc()).all(db).await?;
    assert_eq!(names(adults), ["Alice", "Dave", "Eve"]);

    let q = User::query().filter(User::AGE.ge(18) & User::NAME.contains("li")).all(db).await?;
    assert_eq!(names(q), ["Alice"]);

    // LIKE is case-sensitive everywhere, ILIKE is not.
    assert!(User::query().filter(User::NAME.contains("LI")).all(db).await?.is_empty());
    assert_eq!(names(User::query().filter(User::NAME.icontains("LI")).all(db).await?), ["Alice"]);
    assert_eq!(names(User::query().filter(User::NAME.starts_with("Da")).all(db).await?), ["Dave"]);
    assert_eq!(
        names(User::query().filter(User::NAME.ends_with("ve")).order_by(User::ID).all(db).await?),
        ["Dave", "Eve"]
    );
    assert_eq!(names(User::query().filter(User::EMAIL.like("_ob@%")).all(db).await?), ["Bob"]);
    // Wildcards in user input are escaped.
    assert!(User::query().filter(User::NAME.contains("%")).all(db).await?.is_empty());
    let prefix = memoire_orm::escape_like("Car");
    assert_eq!(names(User::query().filter(User::NAME.like(format!("{prefix}%"))).all(db).await?), ["Carol"]);
    assert!(User::query().filter(User::NAME.like("Car\\%")).all(db).await?.is_empty());
    assert_eq!(User::query().filter(User::NAME.not_like("%a%")).count(db).await?, 3);

    let q = User::query().filter(User::ID.is_in([1, 3, 5])).order_by(User::ID.desc()).all(db).await?;
    assert_eq!(names(q), ["Eve", "Carol", "Alice"]);
    assert!(User::query().filter(User::ID.is_in(Vec::<i64>::new())).all(db).await?.is_empty());
    assert_eq!(User::query().filter(User::ID.not_in(Vec::<i64>::new())).count(db).await?, 5);
    assert_eq!(
        names(User::query().filter(User::AGE.between(20, 31)).order_by(User::ID).all(db).await?),
        ["Alice", "Eve"]
    );
    assert_eq!(names(User::query().filter(User::AGE.is_null()).all(db).await?), ["Carol"]);
    assert_eq!(User::query().filter(User::AGE.is_not_null()).count(db).await?, 4);
    // NULL never matches a comparison (three-valued logic).
    assert_eq!(User::query().filter(!User::AGE.gt(18)).count(db).await?, 1);
    assert_eq!(
        names(User::query().filter(User::NAME.eq("Bob") | User::NAME.eq("Eve")).order_by(User::ID).all(db).await?),
        ["Bob", "Eve"]
    );
    assert_eq!(
        names(User::query().filter(User::SCORE.gt(2.0)).order_by(User::SCORE.desc()).all(db).await?),
        ["Dave", "Carol", "Bob"]
    );

    // NULLS FIRST / LAST.
    let q = User::query().order_by(User::AGE.asc().nulls_first()).all(db).await?;
    assert_eq!(q[0].name, "Carol");
    let q = User::query().order_by(User::AGE.asc().nulls_last()).all(db).await?;
    assert_eq!(q[4].name, "Carol");
    assert_eq!(q[0].name, "Bob");

    // Pagination.
    let page = User::query().order_by(User::ID).paginate(db, 2, 2).await?;
    assert_eq!((page.total, page.total_pages(), page.has_next(), page.has_prev()), (5, 3, true, true));
    assert_eq!(names(page.items), ["Carol", "Dave"]);
    assert_eq!(names(User::query().order_by(User::ID).offset(3).all(db).await?), ["Dave", "Eve"]);

    // first / one / exists.
    assert_eq!(
        User::query().order_by(User::AGE.desc().nulls_last()).first(db).await?.map(|u| u.name),
        Some("Dave".into())
    );
    assert!(User::query().filter(User::NAME.eq("Nobody")).one(db).await.unwrap_err().is_not_found());
    assert!(User::query().filter(User::NAME.eq("Eve")).exists(db).await?);
    assert!(!User::query().filter(User::NAME.eq("Nobody")).exists(db).await?);

    // Aggregates.
    assert_eq!(User::query().sum(User::AGE, db).await?, Some(115));
    assert_eq!(User::query().filter(User::NAME.eq("Nobody")).sum(User::AGE, db).await?, None);
    assert_eq!(User::query().max(User::AGE, db).await?, Some(45));
    assert_eq!(User::query().min(User::NAME, db).await?, Some("Alice".to_owned()));
    let avg = User::query().avg(User::AGE, db).await?.unwrap();
    assert!((avg - 28.75).abs() < 1e-9, "avg = {avg}");
    assert_eq!(User::query().sum(User::SCORE, db).await?, Some(11.5));

    // Typed projections.
    let pairs: Vec<(i64, String)> =
        User::query().columns((User::ID, User::NAME)).order_by(User::ID).limit(2).all(db).await?;
    assert_eq!(pairs, vec![(1, "Alice".to_owned()), (2, "Bob".to_owned())]);
    let ages: Vec<Option<i32>> = User::query().columns(User::AGE).order_by(User::ID).all(db).await?;
    assert_eq!(ages, vec![Some(31), Some(17), None, Some(45), Some(22)]);
    let distinct_active: Vec<bool> = User::query().columns(User::ACTIVE).distinct().all(db).await?;
    assert_eq!(distinct_active, vec![true]);
    assert_eq!(User::query().columns(User::ACTIVE).distinct().count(db).await?, 1);
    Ok(())
}

async fn updates_and_deletes<E: Executor>(db: &E) -> Result<()> {
    seed(db).await?;
    let n = User::update_many()
        .set(User::ACTIVE, false)
        .increment(User::AGE, 1)
        .filter(User::AGE.lt(30))
        .execute(db)
        .await?;
    assert_eq!(n, 2);
    assert_eq!(User::get(db, 2).await?.age, Some(18));
    assert!(!User::get(db, 5).await?.active);
    assert!(User::get(db, 1).await?.active);

    // Assigning a column twice keeps the last assignment.
    User::update_many().set(User::AGE, 99).set_null(User::AGE).filter(User::NAME.eq("Dave")).execute(db).await?;
    assert_eq!(User::get(db, 4).await?.age, None);
    User::update_many().set(User::AGE, 50).filter(User::NAME.eq("Dave")).execute(db).await?;
    assert_eq!(User::get(db, 4).await?.age, Some(50));
    User::update_many().set(User::NAME, "Dave II").filter(User::ID.eq(4)).execute(db).await?;
    assert_eq!(User::get(db, 4).await?.name, "Dave II");

    // Unique violation in a multi-row update leaves the table untouched.
    let err = User::update_many().set(User::EMAIL, "same@example.com").execute(db).await.unwrap_err();
    assert!(err.is_unique_violation(), "{err:?}");
    assert_eq!(User::get(db, 1).await?.email, "alice@example.com");

    let deleted = User::delete_many().filter(User::ACTIVE.eq(false)).execute(db).await?;
    assert_eq!(deleted, 2);
    assert_eq!(User::count(db).await?, 3);
    assert_eq!(User::delete_many().execute(db).await?, 3);
    assert_eq!(User::count(db).await?, 0);
    Ok(())
}

async fn joins_and_groups<E: Executor>(db: &E) -> Result<()> {
    seed(db).await?;
    for (uid, title, views) in [(1, "Hello", 10), (1, "Rust", 30), (2, "Bob's", 5), (4, "Dave", 0)] {
        Post { id: 0, user_id: uid, title: title.into(), views }.insert(db).await?;
    }

    let rows: Vec<(User, Post)> =
        User::query().inner_join::<Post>(Post::USER_ID.eq_col(User::ID)).order_by(Post::ID.asc()).all(db).await?;
    assert_eq!(rows.len(), 4);
    assert_eq!((rows[1].0.name.as_str(), rows[1].1.title.as_str()), ("Alice", "Rust"));

    let rows: Vec<(User, Option<Post>)> = User::query()
        .left_join::<Post>(Post::USER_ID.eq_col(User::ID))
        .filter(User::NAME.is_in(["Carol", "Bob"]))
        .order_by(User::ID)
        .all(db)
        .await?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1.as_ref().map(|p| p.title.as_str()), Some("Bob's"));
    assert_eq!(rows[1].1, None);

    // Join used only for filtering.
    let writers = User::query()
        .join::<Post>(JoinKind::Inner, Post::USER_ID.eq_col(User::ID))
        .filter(Post::VIEWS.ge(5))
        .distinct()
        .order_by(User::ID)
        .all(db)
        .await?;
    assert_eq!(writers.iter().map(|u| u.id).collect::<Vec<_>>(), vec![1, 2]);

    // GROUP BY report decoded into a #[derive(FromRow)] struct.
    let stats: Vec<UserStats> = Post::query()
        .group_by(Post::USER_ID)
        .having(count_all().gt(Value::I64(1)))
        .select_exprs([Post::USER_ID.expr(), count_all(), Post::VIEWS.sum()])
        .all(db)
        .await?;
    assert_eq!(stats, vec![UserStats { user_id: 1, posts: 2, total_views: Some(40) }]);

    let per_user: Vec<(i64, i64)> = Post::query()
        .group_by(Post::USER_ID)
        .select_exprs([Post::USER_ID.expr(), count_all()])
        .order_by(Post::USER_ID.asc())
        .all(db)
        .await?;
    assert_eq!(per_user, vec![(1, 2), (2, 1), (4, 1)]);

    // A join with a projection of columns from both tables.
    let titles: Vec<(String, String)> = Post::query()
        .inner_join::<User>(User::ID.eq_col(Post::USER_ID))
        .columns((User::NAME, Post::TITLE))
        .filter(Post::VIEWS.gt(5))
        .order_by(Post::VIEWS.desc())
        .all(db)
        .await?;
    assert_eq!(titles, vec![("Alice".into(), "Rust".into()), ("Alice".into(), "Hello".into())]);
    Ok(())
}

async fn relations<E: Executor>(db: &E) -> Result<()> {
    let users = seed(db).await?;
    let mut posts = vec![
        Post { id: 0, user_id: 1, title: "Hello".into(), views: 120 },
        Post { id: 0, user_id: 1, title: "Rust".into(), views: 30 },
        Post { id: 0, user_id: 2, title: "Bob's".into(), views: 5 },
        Post { id: 0, user_id: 4, title: "Dave's".into(), views: 50 },
    ];
    for p in &mut posts {
        p.insert(db).await?;
    }

    // find_many: one query, missing keys skipped.
    let mut found = User::find_many(db, [5, 1, 42, 1]).await?;
    found.sort_by_key(|u| u.id);
    assert_eq!(found.iter().map(|u| u.id).collect::<Vec<_>>(), vec![1, 5]);
    assert!(User::find_many(db, Vec::<i64>::new()).await?.is_empty());
    assert_eq!(Tag::find_many(db, ["nope"]).await?, vec![]);

    // belonging_to / find_parent.
    let alice_posts = Post::belonging_to(&users[0]).order_by(Post::ID).all(db).await?;
    assert_eq!(alice_posts, posts[..2].to_vec());
    assert_eq!(Post::belonging_to(&users[2]).count(db).await?, 0);
    assert_eq!(posts[2].find_parent::<User, _>(db).await?, Some(users[1].clone()));
    assert!(users[0].find_parent::<Post, _>(db).await.unwrap_err().to_string().contains("no column"));

    // load_children: aligned with the parents, children in query order.
    let children: Vec<Vec<Post>> = users.load_children(db).await?;
    assert_eq!(children.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1, 0, 1, 0]);
    let popular =
        users.load_children_with(db, Post::query().filter(Post::VIEWS.ge(30)).order_by(Post::VIEWS.desc())).await?;
    assert_eq!(popular[0].iter().map(|p| p.title.as_str()).collect::<Vec<_>>(), vec!["Hello", "Rust"]);
    assert!(popular[1].is_empty());
    let none: Vec<Vec<Post>> = Vec::<User>::new().load_children(db).await?;
    assert!(none.is_empty());

    // load_parent: one parent per child.
    let authors: Vec<Option<User>> = posts.load_parent(db).await?;
    let names: Vec<_> = authors.iter().map(|a| a.as_ref().map(|u| u.name.as_str())).collect();
    assert_eq!(names, vec![Some("Alice"), Some("Alice"), Some("Bob"), Some("Dave")]);

    // load_via: many-to-many through post_tags.
    for (slug, label) in [("rust", "Rust"), ("orm", "ORM"), ("news", "News")] {
        Tag { slug: slug.into(), label: label.into() }.insert(db).await?;
    }
    for (post_id, slug) in [(1, "rust"), (1, "orm"), (2, "rust"), (4, "news")] {
        PostTag { id: 0, post_id, tag_slug: slug.into() }.insert(db).await?;
    }
    let tags: Vec<Vec<Tag>> = posts.load_via::<PostTag, Tag>(db).await?;
    let slugs: Vec<Vec<&str>> = tags.iter().map(|t| t.iter().map(|t| t.slug.as_str()).collect()).collect();
    assert_eq!(slugs, vec![vec!["rust", "orm"], vec!["rust"], vec![], vec!["news"]]);
    let tagged: Vec<Vec<Post>> = Tag::all(db).await?.load_via::<PostTag, Post>(db).await?;
    assert_eq!(tagged.iter().map(Vec::len).sum::<usize>(), 4);

    // update_only: the other columns keep the value stored in the database.
    let mut stale = User::get(db, 1).await?;
    User::update_many().set(User::SCORE, 7.0).filter(User::ID.eq(1)).execute(db).await?;
    stale.name = "Alicia".into();
    stale.age = None;
    stale.score = 0.0;
    stale.update_only(db, (User::NAME, User::AGE)).await?;
    let fresh = User::get(db, 1).await?;
    assert_eq!((fresh.name.as_str(), fresh.age, fresh.score), ("Alicia", None, 7.0));
    stale.update_only(db, User::NAME).await?;
    let mut ghost = stale.clone();
    ghost.id = 999;
    assert!(ghost.update_only(db, User::NAME).await.unwrap_err().is_not_found());

    // paginate (count and page run concurrently).
    let page = User::query().order_by(User::ID).paginate(db, 2, 2).await?;
    assert_eq!((page.total, page.items.iter().map(|u| u.id).collect::<Vec<_>>()), (5, vec![3, 4]));
    Ok(())
}

async fn composite_indexes<D: Database>(db: &D) -> Result<()> {
    let dup = |e: Error| matches!(&e, Error::UniqueViolation { column, .. } if column == "user_id, team");
    for m in [member(1, Some("red"), "admin"), member(2, Some("red"), "dev"), member(1, Some("blue"), "dev")] {
        m.clone().insert(db).await?;
    }
    // The same pair again is refused; one column alone may repeat.
    let err = member(1, Some("red"), "dev").insert(db).await.unwrap_err();
    assert!(dup(err), "insert");
    // A NULL in one of the columns lifts the constraint, like SQL.
    member(1, None, "guest").insert(db).await?;
    member(1, None, "guest").insert(db).await?;
    assert_eq!(Membership::count(db).await?, 5);

    // Updates that would create a duplicate pair are refused, whole.
    let err = Membership::update_many()
        .set(Membership::TEAM, Some("red".to_owned()))
        .filter(Membership::TEAM.eq("blue"))
        .execute(db)
        .await
        .unwrap_err();
    assert!(dup(err), "update_many");
    let mut blue = Membership::query().filter(Membership::TEAM.eq("blue")).one(db).await?;
    blue.team = Some("red".into());
    assert!(dup(blue.update(db).await.unwrap_err()), "update");
    blue.team = Some("green".into());
    blue.update(db).await?;

    // A batch holding a duplicate pair is refused as a whole.
    let batch = [member(3, Some("red"), "dev"), member(3, Some("red"), "ops")];
    assert!(dup(Membership::insert_many(db, &batch).await.unwrap_err()), "insert_many");
    assert_eq!(Membership::count(db).await?, 5);

    // Upsert by primary key keeps working next to the unique pair.
    blue.role = "lead".into();
    blue.upsert(db).await?;
    let mut clash = member(2, Some("red"), "x");
    clash.id = 999;
    assert!(dup(clash.upsert(db).await.unwrap_err()), "upsert");

    // Inside a transaction too.
    let tx = db.begin().await?;
    assert!(dup(member(2, Some("red"), "dev").insert(&tx).await.unwrap_err()), "transaction");
    tx.rollback().await?;

    // Equality on every column of an index (a lookup on the in-memory engine).
    let reds = Membership::query()
        .filter(Membership::TEAM.eq("red") & Membership::ROLE.eq("admin"))
        .columns(Membership::USER_ID)
        .all(db)
        .await?;
    assert_eq!(reds, vec![1]);
    let mut ids: Vec<i64> =
        Membership::query().filter(Membership::USER_ID.eq(1)).columns(Membership::ID).all(db).await?;
    ids.sort();
    assert_eq!(ids.len(), 4);

    // Deleting a row frees its pair.
    Membership::delete_many().filter(Membership::TEAM.eq("red") & Membership::USER_ID.eq(2)).execute(db).await?;
    member(2, Some("red"), "back").insert(db).await?;
    Ok(())
}

async fn full_text<E: Executor>(db: &E) -> Result<()> {
    let bodies = [
        Some("An ORM for Rust!"),
        Some("Rusty nails and orm-tools"),
        Some("Café CRÈME, déjà vu"),
        None,
        Some("rust RUST Rust"),
        Some("user_name, e-mail alice@rust.dev, v3.14"),
    ];
    for (i, body) in bodies.iter().enumerate() {
        Article { id: 0, title: format!("a{}", i + 1), body: body.map(Into::into) }.insert(db).await?;
    }
    async fn ids<E: Executor>(db: &E, filter: Expr) -> Result<Vec<i64>> {
        Article::query().filter(filter).order_by(Article::ID).columns(Article::ID).all(db).await
    }
    // Whole words, every word, any case; "Rusty" is not "rust".
    assert_eq!(ids(db, Article::BODY.matches("rust orm")).await?, vec![1]);
    assert_eq!(ids(db, Article::BODY.matches("RUST")).await?, vec![1, 5, 6]);
    assert_eq!(ids(db, Article::BODY.matches("nails ORM")).await?, vec![2]);
    // Letters beyond ASCII, upper and lower case.
    assert_eq!(ids(db, Article::BODY.matches("café crème")).await?, vec![3]);
    assert_eq!(ids(db, Article::BODY.matches("DÉJÀ")).await?, vec![3]);
    // Punctuation and `_` separate words.
    assert_eq!(ids(db, Article::BODY.matches("user mail 14")).await?, vec![6]);
    // No word: nothing matches; a missing word: nothing matches.
    assert!(ids(db, Article::BODY.matches("")).await?.is_empty());
    assert!(ids(db, Article::BODY.matches(" !? ")).await?.is_empty());
    assert!(ids(db, Article::BODY.matches("rust python")).await?.is_empty());
    // NOT: texts missing a word (a NULL body is neither).
    assert_eq!(ids(db, !Article::BODY.matches("rust")).await?, vec![2, 3]);
    // Combined with other conditions, counted, used by update and delete.
    assert_eq!(ids(db, Article::BODY.matches("rust") & Article::TITLE.ne("a5")).await?, vec![1, 6]);
    assert_eq!(Article::query().filter(Article::BODY.matches("rust")).count(db).await?, 3);
    let n = Article::update_many()
        .set(Article::BODY, Some("rust and ORM".to_owned()))
        .filter(Article::BODY.matches("nails"))
        .execute(db)
        .await?;
    assert_eq!(n, 1);
    assert_eq!(ids(db, Article::BODY.matches("rust orm")).await?, vec![1, 2]);
    assert!(ids(db, Article::BODY.matches("nails")).await?.is_empty());
    Article::delete_many().filter(Article::BODY.matches("orm")).execute(db).await?;
    assert_eq!(ids(db, Article::BODY.matches("rust")).await?, vec![5, 6]);
    Ok(())
}

async fn events<D: Database>(db: &D) -> Result<()> {
    use memoire_orm::events::{Change, ChangeKind, Observed};
    let obs = Observed::new(db);
    let (sub, rx) = obs.channel();
    let drain = || rx.try_iter().collect::<Vec<Change>>();

    let mut u = user("ev@x.dev", "Ev", Some(1), 0.0);
    u.insert(&obs).await?;
    let c = drain();
    assert_eq!(c.len(), 1);
    assert_eq!((c[0].table.as_str(), c[0].kind, c[0].rows), ("users", ChangeKind::Insert, 1));
    assert_eq!(c[0].key.as_ref().and_then(Value::as_i64), Some(u.id), "generated key");
    let row = c[0].row.as_ref().expect("the row");
    assert_eq!(row[0].as_i64(), Some(u.id));
    assert_eq!(row[2].as_str(), Some("Ev"));

    u.name = "Eve".into();
    u.update(&obs).await?;
    u.update_only(&obs, User::AGE).await?;
    u.upsert(&obs).await?;
    let kinds: Vec<ChangeKind> = drain().iter().map(|c| c.kind).collect();
    assert_eq!(kinds, vec![ChangeKind::Update, ChangeKind::Update, ChangeKind::Upsert]);

    // Reads and writes that change nothing are silent.
    User::get(&obs, u.id).await?;
    User::update_many().set(User::AGE, 3).filter(User::ID.eq(-1)).execute(&obs).await?;
    let mut ghost = u.clone();
    ghost.id = 999_999;
    assert!(!ghost.delete(&obs).await?);
    assert!(drain().is_empty());

    User::insert_many(&obs, &[user("e1@x.dev", "E1", None, 0.0), user("e2@x.dev", "E2", None, 0.0)]).await?;
    let n = User::update_many().set(User::ACTIVE, false).filter(User::NAME.starts_with("E")).execute(&obs).await?;
    let c = drain();
    assert_eq!(
        c.iter().map(|c| (c.kind, c.rows)).collect::<Vec<_>>(),
        vec![(ChangeKind::Insert, 2), (ChangeKind::Update, n)]
    );
    assert!(c.iter().all(|c| c.key.is_none() && c.row.is_none()));

    // A transaction announces its changes when it commits, never when it rolls back.
    let tx = obs.begin().await?;
    user("t1@x.dev", "T1", None, 0.0).insert(&tx).await?;
    assert!(drain().is_empty(), "nothing before commit");
    tx.commit().await?;
    assert_eq!(drain().iter().map(|c| c.kind).collect::<Vec<_>>(), vec![ChangeKind::Insert]);
    let tx = obs.begin().await?;
    user("t2@x.dev", "T2", None, 0.0).insert(&tx).await?;
    tx.rollback().await?;
    assert!(drain().is_empty(), "nothing after rollback");

    assert!(u.delete(&obs).await?);
    let c = drain();
    assert_eq!((c[0].kind, c[0].key.as_ref().and_then(Value::as_i64)), (ChangeKind::Delete, Some(u.id)));

    // Dropping the subscription stops the notifications.
    assert_eq!(obs.listener_count(), 1);
    drop(sub);
    assert_eq!(obs.listener_count(), 0);
    user("late@x.dev", "Late", None, 0.0).insert(&obs).await?;
    assert!(drain().is_empty());
    Ok(())
}

async fn transactions<D: Database>(db: &D) -> Result<()> {
    let tx = db.begin().await?;
    let mut a = user("tx1@example.com", "Tx One", None, 0.0);
    a.insert(&tx).await?;
    assert_eq!(User::count(&tx).await?, 1);
    tx.commit().await?;
    assert_eq!(User::count(db).await?, 1);

    let tx = db.begin().await?;
    user("tx2@example.com", "Tx Two", None, 0.0).insert(&tx).await?;
    User::delete_many().filter(User::ID.eq(a.id)).execute(&tx).await?;
    tx.rollback().await?;
    assert_eq!(User::count(db).await?, 1);
    assert!(User::find(db, a.id).await?.is_some());

    {
        let tx = db.begin().await?;
        user("tx3@example.com", "Tx Three", None, 0.0).insert(&tx).await?;
        // dropped without commit
    }
    // Give asynchronous rollbacks (PostgreSQL) a chance to run.
    for _ in 0..50 {
        if User::count(db).await? == 1 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(User::count(db).await?, 1);
    Ok(())
}

async fn transaction_helper<D: Database>(db: &D) -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Ok: committed, and the value comes back.
    let id = db
        .transaction(|tx| {
            Box::pin(async move {
                let mut a = user("helper1@example.com", "Helper One", None, 0.0);
                a.insert(tx).await?;
                Ok(a.id)
            })
        })
        .await?;
    assert!(User::find(db, id).await?.is_some());

    // Err: rolled back, the error comes back, no retry.
    let calls = Arc::new(AtomicU32::new(0));
    let err = db
        .transaction(|tx| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                user("helper2@example.com", "Helper Two", None, 0.0).insert(tx).await?;
                Err::<(), _>(Error::Validation("no".into()))
            })
        })
        .await
        .unwrap_err();
    assert!(err.is_validation());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(User::query().filter(User::EMAIL.eq("helper2@example.com")).count(db).await?, 0);

    // Conflict: run again from the start, the failed attempts leave nothing.
    let calls = Arc::new(AtomicU32::new(0));
    db.transaction(|tx| {
        let calls = calls.clone();
        Box::pin(async move {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            user(&format!("helper3-{n}@example.com"), "Helper Three", None, 0.0).insert(tx).await?;
            if n < 2 { Err(Error::Conflict("users".into())) } else { Ok(()) }
        })
    })
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(User::query().filter(User::NAME.eq("Helper Three")).count(db).await?, 1);
    assert!(User::query().filter(User::EMAIL.eq("helper3-2@example.com")).exists(db).await?);

    // Conflicts forever: gives up after TRANSACTION_ATTEMPTS.
    let calls = Arc::new(AtomicU32::new(0));
    let err = db
        .transaction(|_tx| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Error::Conflict("users".into()))
            })
        })
        .await
        .unwrap_err();
    assert!(err.is_conflict());
    assert_eq!(calls.load(Ordering::SeqCst), memoire_orm::TRANSACTION_ATTEMPTS);
    Ok(())
}

mod push_models {
    use memoire_orm::prelude::*;

    /// The first version of a model.
    #[derive(Model, Debug, Clone, PartialEq)]
    #[memoire(table = "push_items")]
    pub struct V1 {
        #[memoire(primary_key, auto_increment)]
        pub id: i64,
        pub name: String,
    }

    /// The same table, after columns and indexes were added to the model.
    #[derive(Model, Debug, Clone, PartialEq)]
    #[memoire(table = "push_items", index(name, score))]
    pub struct V2 {
        #[memoire(primary_key, auto_increment)]
        pub id: i64,
        pub name: String,
        #[memoire(unique)]
        pub code: Option<String>,
        #[memoire(index, default = "7")]
        pub score: i32,
    }

    /// A column existing rows could not fill.
    #[derive(Model, Debug, Clone, PartialEq)]
    #[memoire(table = "push_items")]
    pub struct V3 {
        #[memoire(primary_key, auto_increment)]
        pub id: i64,
        pub name: String,
        pub required: String,
    }
}

async fn push_schema<E: Executor>(db: &E) -> Result<()> {
    use push_models::{V1, V2, V3};
    db.drop_table_if_exists::<V1>().await?;

    let report = db.push::<V1>().await?;
    assert!(report.created);
    assert!(db.push::<V1>().await?.is_unchanged());
    let mut old = V1 { id: 0, name: "old".into() };
    old.insert(db).await?;

    if db.dialect().is_none() {
        // In-memory, Redis, MongoDB: the table exists, nothing to alter.
        db.drop_table::<V1>().await?;
        return Ok(());
    }

    // A column that existing rows could not fill: refused, nothing changes.
    let err = db.push::<V3>().await.unwrap_err();
    assert!(err.to_string().contains("push_items.required"), "{err}");

    let report = db.push::<V2>().await?;
    assert!(!report.created);
    assert_eq!(report.added_columns, vec!["code", "score"]);
    let mut indexes = report.added_indexes.clone();
    indexes.sort();
    assert_eq!(indexes.len(), 3, "{indexes:?}");
    assert!(db.push::<V2>().await?.is_unchanged());

    // The old row reads with the new model: NULL and the default.
    let row = V2::find(db, old.id).await?.unwrap();
    assert_eq!(row, V2 { id: old.id, name: "old".into(), code: None, score: 7 });
    // The unique index added with the column works.
    let mut a = V2 { id: 0, name: "a".into(), code: Some("X".into()), score: 1 };
    a.insert(db).await?;
    let mut b = V2 { id: 0, name: "b".into(), code: Some("X".into()), score: 2 };
    assert!(b.insert(db).await.unwrap_err().is_unique_violation());

    db.drop_table::<V2>().await?;
    Ok(())
}

async fn bulk<E: Executor>(db: &E) -> Result<()> {
    let users: Vec<User> = (0..2500)
        .map(|i| user(&format!("user{i}@bulk.dev"), &format!("User {i:04}"), Some(i % 90), i as f64))
        .collect();
    assert_eq!(User::insert_many(db, &users).await?, 2500);
    assert_eq!(User::count(db).await?, 2500);
    assert_eq!(User::query().filter(User::AGE.eq(10)).count(db).await?, 28);
    let top = User::query().order_by(User::SCORE.desc()).limit(3).all(db).await?;
    assert_eq!(top.iter().map(|u| u.score).collect::<Vec<_>>(), vec![2499.0, 2498.0, 2497.0]);
    let page = User::query().order_by(User::NAME.asc()).offset(10).limit(2).all(db).await?;
    assert_eq!(page.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["User 0010", "User 0011"]);
    // A failing batch inserts nothing.
    let dupes = vec![user("x@bulk.dev", "X", None, 0.0), user("x@bulk.dev", "X2", None, 0.0)];
    assert!(User::insert_many(db, &dupes).await.unwrap_err().is_unique_violation());
    assert_eq!(User::count(db).await?, 2500);
    Ok(())
}

async fn errors<E: Executor>(db: &E) -> Result<()> {
    let mut a = user("dup@example.com", "Dup", None, 0.0);
    a.insert(db).await?;
    let err = user("dup@example.com", "Dup 2", None, 0.0).insert(db).await.unwrap_err();
    match &err {
        Error::UniqueViolation { column, .. } => assert_eq!(column, "email"),
        other => panic!("expected a unique violation, got {other:?}"),
    }
    // An upsert matches on the primary key only: a new key whose unique value
    // belongs to another row fails and leaves that row untouched.
    let mut b = user("dup@example.com", "Intruder", Some(7), 1.0);
    b.id = a.id + 1000;
    assert!(b.upsert(db).await.unwrap_err().is_unique_violation());
    let kept = User::get(db, a.id).await?;
    assert_eq!((kept.name.as_str(), kept.age), ("Dup", None));
    assert!(User::find(db, b.id).await?.is_none());
    // Upserting the same key still updates, and a unique clash on update fails.
    let mut c = user("other@example.com", "Other", None, 0.0);
    c.insert(db).await?;
    a.name = "Dup v2".into();
    a.upsert(db).await?;
    assert_eq!(User::get(db, a.id).await?.name, "Dup v2");
    c.email = "dup@example.com".into();
    assert!(c.upsert(db).await.unwrap_err().is_unique_violation());
    assert_eq!(User::get(db, c.id).await?.email, "other@example.com");
    c.delete(db).await?;
    let mut tag = Tag { slug: "k".into(), label: "K".into() };
    tag.insert(db).await?;
    assert!(tag.insert(db).await.unwrap_err().is_unique_violation());

    db.drop_table::<PostTag>().await?;
    db.drop_table::<Tag>().await?;
    assert!(matches!(Tag::count(db).await.unwrap_err(), Error::NoSuchTable(t) if t == "tags"));
    assert!(matches!(db.create_table::<User>().await.unwrap_err(), Error::TableExists(t) if t == "users"));
    db.create_table_if_not_exists::<User>().await?;
    db.create_table::<Tag>().await?;
    Ok(())
}

async fn migrations<D: Database>(db: &D) -> Result<()> {
    db.drop_table_if_exists::<PostTag>().await?;
    db.drop_table_if_exists::<Post>().await?;
    db.drop_table_if_exists::<User>().await?;
    let m = Migrator::new()
        .add(Migration::new(2, "create_posts").create_table::<Post>())
        .add(Migration::new(1, "create_users").create_table::<User>());
    assert_eq!(m.run(db).await?, vec![1, 2]);
    assert!(m.run(db).await?.is_empty());
    assert_eq!(m.applied(db).await?.len(), 2);

    // A failing migration is rolled back and reported.
    let bad = m.clone().add(Migration::new(3, "broken").create_table::<User>());
    let err = bad.run(db).await.unwrap_err();
    assert!(matches!(err, Error::Migration { version: 3, .. }), "{err:?}");
    assert_eq!(bad.pending(db).await?.len(), 1);
    Ok(())
}

/// Every supported column type, on every backend.
#[cfg(all(feature = "chrono", feature = "uuid", feature = "json"))]
pub mod typed {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
    use memoire_orm::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
    pub struct Meta {
        pub tags: Vec<String>,
        pub score: u32,
    }

    #[derive(Model, Debug, Clone, PartialEq)]
    #[memoire(table = "typed_rows")]
    pub struct Typed {
        #[memoire(primary_key)]
        pub id: uuid::Uuid,
        pub flag: bool,
        pub small: i16,
        pub int: i32,
        pub big: i64,
        pub real: f32,
        pub double: f64,
        pub text: String,
        pub bytes: Vec<u8>,
        pub maybe: Option<String>,
        #[memoire(index)]
        pub at: DateTime<Utc>,
        pub local: NaiveDateTime,
        pub day: NaiveDate,
        pub time: NaiveTime,
        pub doc: serde_json::Value,
        pub meta: Json<Meta>,
    }

    fn row(n: u32) -> Typed {
        // Millisecond precision: the finest resolution shared by every backend.
        let at =
            Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap() + chrono::Duration::milliseconds(n as i64 * 1_500 + 7);
        Typed {
            id: uuid::Uuid::from_u128(0x1000 + n as u128),
            flag: n.is_multiple_of(2),
            small: -(n as i16),
            int: n as i32 * 1000,
            big: i64::MAX - n as i64,
            real: 0.5 * n as f32,
            double: 1.25 * n as f64,
            text: format!("ligne n°{n} — ünïcødé ✓"),
            bytes: vec![0, 255, n as u8, 0x80],
            maybe: (!n.is_multiple_of(3)).then(|| format!("maybe {n}")),
            at,
            local: at.naive_utc() + chrono::Duration::hours(2),
            day: NaiveDate::from_ymd_opt(2026, 3, n + 1).unwrap(),
            time: NaiveTime::from_hms_milli_opt(8 + n, 30, 15, 250).unwrap(),
            doc: serde_json::json!({ "n": n, "nested": { "ok": true }, "list": [1, 2, 3] }),
            meta: Json(Meta { tags: vec!["a".into(), format!("t{n}")], score: n * 10 }),
        }
    }

    pub async fn run<D: Database>(db: &D) -> Result<()> {
        db.drop_table_if_exists::<Typed>().await?;
        db.create_table::<Typed>().await?;
        let rows: Vec<Typed> = (0..12).map(row).collect();
        for r in &rows[..6] {
            r.clone().insert(db).await?;
        }
        Typed::insert_many(db, &rows[6..]).await?;

        // Exact round trip of every type.
        for r in &rows {
            assert_eq!(&Typed::get(db, r.id).await?, r);
        }

        // Ordering and comparisons on timestamps, dates and times.
        let t5 = rows[5].at;
        let later = Typed::query().filter(Typed::AT.gt(t5)).order_by(Typed::AT.desc()).all(db).await?;
        assert_eq!(later.iter().map(|r| r.int).collect::<Vec<_>>(), vec![11000, 10000, 9000, 8000, 7000, 6000]);
        assert_eq!(Typed::query().filter(Typed::LOCAL.le(rows[1].local)).count(db).await?, 2);
        assert_eq!(Typed::query().filter(Typed::DAY.between(rows[2].day, rows[4].day)).count(db).await?, 3);
        assert_eq!(Typed::query().filter(Typed::TIME.ge(rows[10].time)).count(db).await?, 2);
        let first = Typed::query().order_by(Typed::AT.asc()).first(db).await?.unwrap();
        assert_eq!(first, rows[0]);

        // Other comparisons.
        assert_eq!(Typed::query().filter(Typed::FLAG.eq(true)).count(db).await?, 6);
        assert_eq!(Typed::query().filter(Typed::SMALL.lt(-9)).count(db).await?, 2);
        assert_eq!(Typed::query().filter(Typed::BIG.eq(i64::MAX)).count(db).await?, 1);
        assert_eq!(Typed::query().filter(Typed::REAL.ge(5.0)).count(db).await?, 2);
        assert_eq!(Typed::query().filter(Typed::MAYBE.is_null()).count(db).await?, 4);
        assert_eq!(Typed::query().filter(Typed::TEXT.contains("n°7 —")).count(db).await?, 1);
        assert_eq!(Typed::query().filter(Typed::ID.is_in([rows[3].id, rows[4].id])).count(db).await?, 2);
        assert_eq!(Typed::query().max(Typed::DAY, db).await?, Some(rows[11].day));
        assert_eq!(Typed::query().sum(Typed::INT, db).await?, Some(66_000));

        // Update and delete by key with a UUID primary key.
        let mut r = rows[0].clone();
        r.meta.0.tags.push("updated".into());
        r.doc = serde_json::json!(["replaced"]);
        r.update(db).await?;
        assert_eq!(Typed::get(db, r.id).await?, r);
        assert!(r.delete(db).await?);
        assert_eq!(Typed::count(db).await?, 11);
        db.drop_table::<Typed>().await?;
        Ok(())
    }
}
