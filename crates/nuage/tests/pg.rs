//! The PostgreSQL protocol end to end, with tokio-postgres (a driver that
//! prepares every statement and reads and writes values in binary).

#![cfg(feature = "server")]

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use nuage::{Engine, EngineOptions, PgOptions, Server, ServerOptions};
use tokio_postgres::{Client, NoTls, error::SqlState};

async fn start(name: &str) -> (Server, std::net::SocketAddr) {
    let dir = std::env::temp_dir().join(format!("nuage-pg-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine: Arc<Engine> = Engine::open(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    let server = Server::new(engine, ServerOptions { token: Some("t0ken".into()), ..ServerOptions::default() });
    let running = server.listen_pg("127.0.0.1:0", PgOptions::default()).await.unwrap();
    let addr = running.local_addr();
    std::mem::forget(running);
    (server, addr)
}

async fn connect(addr: std::net::SocketAddr, password: &str, dbname: &str) -> Result<Client, tokio_postgres::Error> {
    let (client, conn) = tokio_postgres::connect(
        &format!("host={} port={} user=anyone password={password} dbname={dbname}", addr.ip(), addr.port()),
        NoTls,
    )
    .await?;
    tokio::spawn(conn);
    Ok(client)
}

#[tokio::test]
async fn drivers_connect_query_and_commit() {
    let (server, addr) = start("main").await;

    // The password is a token.
    let refused = connect(addr, "wrong", "nuage").await.err().unwrap();
    assert_eq!(refused.code(), Some(&SqlState::INVALID_PASSWORD));
    let db = connect(addr, "t0ken", "nuage").await.unwrap();

    // What drivers ask first.
    let v: String = db.query_one("SELECT version()", &[]).await.unwrap().get(0);
    assert!(v.starts_with("PostgreSQL 16") && v.contains("Nuage DB"), "{v}");
    db.batch_execute("SET application_name = 'test'; SET extra_float_digits = 3").await.unwrap();
    let ss = db.simple_query("SHOW standard_conforming_strings").await.unwrap();
    assert!(matches!(&ss[1], tokio_postgres::SimpleQueryMessage::Row(r) if r.get(0) == Some("on")));

    db.batch_execute(
        "CREATE TABLE items (id SERIAL PRIMARY KEY, name TEXT NOT NULL UNIQUE, qty INTEGER, price DOUBLE PRECISION,
         ok BOOLEAN, data BYTEA, at TIMESTAMPTZ, day DATE, ref UUID)",
    )
    .await
    .unwrap();

    // Parameters in binary, each to the type of its column; RETURNING.
    let at: DateTime<Utc> = "2026-11-13T10:30:00.250Z".parse().unwrap();
    let day = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();
    let r = uuid::Uuid::from_u128(42);
    let insert = "INSERT INTO items (name, qty, price, ok, data, at, day, ref) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id";
    let id: i64 =
        db.query_one(insert, &[&"apple", &3i64, &1.5f64, &true, &vec![0u8, 255], &at, &day, &r]).await.unwrap().get(0);
    assert_eq!(id, 1);
    db.execute(
        insert,
        &[
            &"pear",
            &None::<i64>,
            &2.0f64,
            &false,
            &None::<Vec<u8>>,
            &None::<DateTime<Utc>>,
            &None::<NaiveDate>,
            &None::<uuid::Uuid>,
        ],
    )
    .await
    .unwrap();

    // Rows back in binary, with their types.
    let row = db
        .query_one("SELECT id, name, qty, price, ok, data, at, day, ref FROM items WHERE name = $1", &[&"apple"])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>("id"), 1);
    assert_eq!(row.get::<_, String>("name"), "apple");
    assert_eq!(row.get::<_, i64>("qty"), 3);
    assert_eq!(row.get::<_, f64>("price"), 1.5);
    assert!(row.get::<_, bool>("ok"));
    assert_eq!(row.get::<_, Vec<u8>>("data"), vec![0, 255]);
    assert_eq!(row.get::<_, DateTime<Utc>>("at"), at);
    assert_eq!(row.get::<_, NaiveDate>("day"), day);
    assert_eq!(row.get::<_, uuid::Uuid>("ref"), r);
    let nulls = db.query_one("SELECT qty, data FROM items WHERE id = $1", &[&2i64]).await.unwrap();
    assert_eq!(nulls.get::<_, Option<i64>>(0), None);
    // An empty result still has typed columns; LIMIT takes a parameter.
    let none =
        db.query("SELECT id, price FROM items WHERE qty > $1 ORDER BY id LIMIT $2", &[&100i64, &5i64]).await.unwrap();
    assert!(none.is_empty());
    let n: i64 = db.query_one("SELECT count(*) FROM items", &[]).await.unwrap().get(0);
    assert_eq!(n, 2);

    // Errors carry PostgreSQL's codes, and the session goes on.
    let dup = db.execute("INSERT INTO items (name) VALUES ($1)", &[&"apple"]).await.unwrap_err();
    assert_eq!(dup.code(), Some(&SqlState::UNIQUE_VIOLATION));
    let missing = db.query("SELECT * FROM nowhere", &[]).await.unwrap_err();
    assert_eq!(missing.code(), Some(&SqlState::UNDEFINED_TABLE));
    assert_eq!(db.query_one("SELECT 1 + 1", &[]).await.unwrap().get::<_, i64>(0), 2);

    // A transaction: rolled back, then committed.
    let mut db = db;
    let tx = db.transaction().await.unwrap();
    tx.execute("UPDATE items SET qty = 0", &[]).await.unwrap();
    tx.rollback().await.unwrap();
    let tx = db.transaction().await.unwrap();
    tx.execute("UPDATE items SET qty = qty + 1 WHERE id = $1", &[&1i64]).await.unwrap();
    // Inside it, a savepoint undoes a part.
    tx.batch_execute("SAVEPOINT s1").await.unwrap();
    tx.execute("DELETE FROM items", &[]).await.unwrap();
    tx.batch_execute("ROLLBACK TO SAVEPOINT s1").await.unwrap();
    tx.commit().await.unwrap();
    let rows = db.query("SELECT qty FROM items ORDER BY id", &[]).await.unwrap();
    assert_eq!((rows.len(), rows[0].get::<_, i64>(0)), (2, 4));

    // A failed statement aborts the transaction until ROLLBACK.
    db.batch_execute("BEGIN").await.unwrap();
    assert!(db.batch_execute("SELECT * FROM nowhere").await.is_err());
    let aborted = db.batch_execute("SELECT 1").await.unwrap_err();
    assert_eq!(aborted.code(), Some(&SqlState::IN_FAILED_SQL_TRANSACTION));
    db.batch_execute("ROLLBACK").await.unwrap();

    // Several statements in one query: all or nothing.
    assert!(
        db.batch_execute("INSERT INTO items (name) VALUES ('kiwi'); INSERT INTO items (name) VALUES ('apple')")
            .await
            .is_err()
    );
    let kiwi = db.query("SELECT id FROM items WHERE name = 'kiwi'", &[]).await.unwrap();
    assert!(kiwi.is_empty(), "the first insert was rolled back with the second");

    // A read-only token reads, never writes.
    let read_only = server.new_app_read_only_token("nuage").unwrap();
    let ro = connect(addr, &read_only, "nuage").await.unwrap();
    assert_eq!(ro.query("SELECT * FROM items", &[]).await.unwrap().len(), 2);
    assert!(ro.execute("DELETE FROM items", &[]).await.is_err());

    // The database name picks a branch.
    server.engine().create_branch("preview", None).unwrap();
    let preview = connect(addr, "t0ken", "preview").await.unwrap();
    preview.execute("DELETE FROM items", &[]).await.unwrap();
    assert_eq!(preview.query("SELECT * FROM items", &[]).await.unwrap().len(), 0);
    assert_eq!(db.query("SELECT * FROM items", &[]).await.unwrap().len(), 2, "main is untouched");
    assert!(connect(addr, "t0ken", "no-such-branch").await.is_err());
}

/// A driver that prepares a statement asks for its columns before it runs
/// it: inside `BEGIN`, asking writes nothing.
#[tokio::test]
async fn describing_a_statement_in_a_transaction_changes_nothing() {
    let (_server, addr) = start("describe").await;
    let db = connect(addr, "t0ken", "nuage").await.unwrap();
    db.batch_execute(
        "CREATE TABLE counters (id SERIAL PRIMARY KEY, name TEXT, n BIGINT NOT NULL DEFAULT 0);
         INSERT INTO counters (name, n) VALUES ('a', 10), ('b', 20)",
    )
    .await
    .unwrap();

    db.batch_execute("BEGIN").await.unwrap();
    // No parameter: the statement would run as it is.
    let n: i64 = db.query_one("UPDATE counters SET n = n + 1 WHERE name = 'a' RETURNING n", &[]).await.unwrap().get(0);
    assert_eq!(n, 11, "run once");
    // Its parameters NULL, the row is one the table accepts.
    let id: i64 = db.query_one("INSERT INTO counters (name) VALUES ($1) RETURNING id", &[&"c"]).await.unwrap().get(0);
    assert_eq!(id, 3);
    // Prepared and never run.
    db.prepare("DELETE FROM counters RETURNING id").await.unwrap();
    let rows = db.query("SELECT name, n FROM counters ORDER BY id", &[]).await.unwrap();
    let seen: Vec<(Option<String>, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(seen, [(Some("a".into()), 11), (Some("b".into()), 20), (Some("c".into()), 0)]);
    db.batch_execute("COMMIT").await.unwrap();

    let rows = db.query("SELECT name, n FROM counters ORDER BY id", &[]).await.unwrap();
    let seen: Vec<(Option<String>, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(seen, [(Some("a".into()), 11), (Some("b".into()), 20), (Some("c".into()), 0)]);
    // Outside a transaction too.
    let n: i64 = db.query_one("UPDATE counters SET n = n + 1 WHERE name = 'b' RETURNING n", &[]).await.unwrap().get(0);
    assert_eq!(n, 21);
}
