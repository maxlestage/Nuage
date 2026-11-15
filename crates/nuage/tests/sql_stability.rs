//! SQL that must neither bring the server down nor answer wrongly: results of
//! subqueries, deep and long statements, functions that build large texts,
//! the memory and the time a statement may take.

use nuage::{Embedded, SqlResult};
use serde_json::{Value as Json, json};

struct Db(Embedded, std::path::PathBuf);

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

impl Db {
    fn open(name: &str) -> Db {
        let dir = std::env::temp_dir().join(format!("nuage-sql-stability-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Db(Embedded::open(&dir).unwrap(), dir)
    }

    fn run(&self, sql: &str, params: &[Json]) -> Vec<SqlResult> {
        self.0.sql(sql, params).unwrap_or_else(|e| panic!("{}\n→ {e}", &sql[..sql.len().min(300)]))
    }

    /// Rows of the last statement as JSON arrays, to compare at a glance.
    fn rows(&self, sql: &str) -> Json {
        let r = self.run(sql, &[]).pop().unwrap();
        Json::Array(r.rows.iter().map(|row| Json::Array(row.iter().map(nuage::sql::value_to_json).collect())).collect())
    }

    fn err(&self, sql: &str) -> String {
        self.0.sql(sql, &[]).expect_err(&sql[..sql.len().min(300)]).to_string()
    }
}

/// Four users and four posts; users 1 and 2 are banned, post 4 is spam.
fn forum(name: &str) -> Db {
    let db = Db::open(name);
    db.run(
        "CREATE TABLE users (id INT PRIMARY KEY, credits INT NOT NULL DEFAULT 0);
         CREATE TABLE posts (id INT PRIMARY KEY, views INT NOT NULL DEFAULT 0);
         CREATE TABLE banned (user_id INT PRIMARY KEY);
         CREATE TABLE spam (post_id INT PRIMARY KEY);
         INSERT INTO users (id) VALUES (1), (2), (3), (4);
         INSERT INTO posts (id) VALUES (1), (2), (3), (4);
         INSERT INTO banned VALUES (1), (2);
         INSERT INTO spam VALUES (4);",
        &[],
    );
    db
}

/// Each statement of a script reads its own subquery: the second `UPDATE` once
/// changed the rows the first one's subquery had named.
#[test]
fn a_subquery_is_never_answered_with_another_one() {
    let db = forum("subqueries");
    let done = db.run(
        "UPDATE users SET credits = credits + 100 WHERE abs(id) > 0 AND id IN (SELECT user_id FROM banned);
         UPDATE posts SET views = views + 100 WHERE abs(id) > 0 AND id IN (SELECT post_id FROM spam)",
        &[],
    );
    assert_eq!((done[0].rows_affected, done[1].rows_affected), (2, 1));
    assert_eq!(db.rows("SELECT id FROM posts WHERE views = 100"), json!([[4]]));
    assert_eq!(db.rows("SELECT id FROM users WHERE credits = 100 ORDER BY id"), json!([[1], [2]]));

    // The same without the function that keeps the filter in the SQL layer.
    db.run(
        "UPDATE users SET credits = credits + 1 WHERE id IN (SELECT user_id FROM banned);
         UPDATE posts SET views = views + 1 WHERE id IN (SELECT post_id FROM spam)",
        &[],
    );
    assert_eq!(db.rows("SELECT id FROM posts WHERE views = 101"), json!([[4]]));

    // A delete after a read of other rows.
    let gone = db.run(
        "SELECT id FROM users WHERE abs(id) > 0 AND id IN (SELECT user_id FROM banned);
         DELETE FROM posts WHERE abs(id) > 0 AND id IN (SELECT post_id FROM spam) RETURNING id",
        &[],
    );
    assert_eq!(gone[1].rows.len(), 1);
    assert_eq!(db.rows("SELECT id FROM posts ORDER BY id"), json!([[1], [2], [3]]));
    db.run("INSERT INTO posts (id) VALUES (4)", &[]);

    // Inside one statement: the arms of a UNION, two subqueries in a list.
    assert_eq!(
        db.rows(
            "SELECT id FROM posts WHERE abs(id) > 0 AND id IN (SELECT user_id FROM banned)
             UNION ALL SELECT id FROM posts WHERE abs(id) > 0 AND id IN (SELECT post_id FROM spam)"
        ),
        json!([[1], [2], [4]])
    );
    assert_eq!(
        db.rows(
            "SELECT (SELECT count(*) FROM posts WHERE abs(id) > 0 AND id IN (SELECT user_id FROM banned)) a,
                    (SELECT count(*) FROM posts WHERE abs(id) > 0 AND id IN (SELECT post_id FROM spam)) b"
        ),
        json!([[2, 1]])
    );
}

/// `x::type(` without its `)` once read past the end of the statement for
/// ever: one request froze the thread that read it.
#[test]
fn a_type_left_open_is_an_error() {
    let db = Db::open("open-type");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            for sql in [
                "SELECT 1::int(",
                "SELECT 1::varchar(10",
                "SELECT CAST(1 AS varchar(",
                "SELECT CAST(1 AS varchar(10)",
                "CREATE TABLE u (a varchar(",
            ] {
                tx.send((sql, db.err(sql))).unwrap();
            }
        });
        for _ in 0..5 {
            let (sql, e) = rx.recv_timeout(std::time::Duration::from_secs(30)).expect("the parser loops");
            assert!(e.contains("SQL: "), "{sql}: {e}");
        }
    });
    assert_eq!(db.rows("SELECT 1::varchar(10), CAST(2 AS numeric(10, 2))"), json!([["1", 2.0]]));
}

/// Functions and operators that build a text from a length the statement
/// gives refuse past 64 MiB, before taking the memory.
#[test]
fn texts_have_a_maximum_size() {
    let db = Db::open("texts");
    for sql in [
        "SELECT length(repeat(repeat('a', 1000000), 1000000))",
        "SELECT length(lpad('x', 9000000000000000000, 'y'))",
        "SELECT length(lpad('x', 2000000000, 'abcdefgh'))",
        "SELECT length(rpad('x', 900000000))",
        "SELECT length(replace(repeat('a', 1000000), 'a', repeat('b', 600)))",
        "SELECT length(repeat('a', 40000000) || repeat('b', 40000000))",
        "SELECT length(concat(repeat('a', 40000000), repeat('b', 40000000)))",
        "SELECT length(concat_ws(',', repeat('a', 40000000), repeat('b', 40000000)))",
        // A character that becomes three: 30 MiB of text, 90 in capitals.
        "SELECT length(upper(repeat('ΐ', 15000000)))",
    ] {
        let e = db.err(sql);
        assert!(e.contains("larger than 64 MiB"), "{sql}: {e}");
    }
    // Aggregates: what they join is counted before it is joined.
    db.run("CREATE TABLE big (id INT PRIMARY KEY, s TEXT); INSERT INTO big VALUES (1, repeat('x', 30000000)), (2, repeat('y', 30000000)), (3, repeat('z', 30000000))", &[]);
    for sql in [
        "SELECT length(string_agg(s, ',')) FROM big",
        "SELECT length(group_concat(s)) FROM big",
        "SELECT array_agg(s) FROM big",
    ] {
        let e = db.err(sql);
        assert!(e.contains("larger than 64 MiB"), "{sql}: {e}");
    }
    // A JSON text whose values take more than its text, read as JSON.
    let e = db.err(
        "SELECT count(*) FROM (SELECT jsonb_array_elements(('[' || repeat('0,', 20000000) || '0]')::json) AS x) AS t",
    );
    assert!(e.contains("larger than 64 MiB"), "{e}");
    // Under the limit, as before.
    assert_eq!(
        db.rows("SELECT length(repeat('ab', 3000000)), lpad('7', 3, '0'), rpad('é', 3, 'àb'), length(string_agg(s, '')) FROM big WHERE id < 3"),
        json!([[6000000, "007", "éàb", 60000000]])
    );
    assert_eq!(
        db.rows("SELECT substr('héllo', 2, 3), left('héllo', 2), right('héllo', 2), substr('abc', -1, 3)"),
        json!([["éll", "hé", "lo", "a"]])
    );
}

/// Ordinary input that a function cannot answer gives an error and leaves
/// the server as it was: these panicked.
#[test]
fn odd_input_gives_an_error() {
    let db = Db::open("odd");
    db.run("CREATE TABLE d (id INT PRIMARY KEY, day DATE); INSERT INTO d VALUES (1, '2026-01-01')", &[]);
    for (sql, says) in [
        ("SELECT strftime('%J', day) FROM d", "strftime()"),
        ("SELECT day + 2147483648 FROM d", "date out of range"),
        ("SELECT date '2026-01-01' + 99999999999", "date out of range"),
        ("SELECT day - 9223372036854775807 FROM d", "date out of range"),
        ("SELECT 1 + _interval()", "unknown function"),
        ("SELECT _same()", "unknown function"),
        ("SELECT _shift(1, 2, 3)", "unknown function"),
        ("SELECT $1000000000000", "up to 65535"),
        ("SELECT ?65536", "up to 65535"),
        ("SELECT abs(-9223372036854775807 - 1)", "overflow"),
        ("SELECT -(-9223372036854775807 - 1)", "overflow"),
        ("SELECT (-9223372036854775807 - 1) / -1", "overflow"),
        ("SELECT (-9223372036854775807 - 1) % -1", "overflow"),
        ("SELECT timestamp '2026-01-01 00:00:00' + INTERVAL '1e300 seconds'", "INTERVAL"),
    ] {
        let e = db.err(sql);
        assert!(e.contains(says), "{sql}: {e}");
    }
    assert_eq!(
        db.rows("SELECT strftime('%Y-%m', day), day + 31, day IS DISTINCT FROM NULL FROM d"),
        json!([["2026-01", "2026-02-01", true]])
    );
}

/// A join without ON is filtered by WHERE as it is built, and counted without
/// being built when only its number is asked.
#[test]
fn cross_joins_are_not_built_for_nothing() {
    let db = Db::open("cross");
    let values: Vec<String> = (1..=1500).map(|i| format!("({i}, {})", i % 7)).collect();
    db.run(&format!("CREATE TABLE t (id INT PRIMARY KEY, g INT); INSERT INTO t VALUES {}", values.join(",")), &[]);
    assert_eq!(db.rows("SELECT count(*) FROM t a CROSS JOIN t b"), json!([[2_250_000]]));
    assert_eq!(
        db.rows("SELECT count(*) FROM t a, t b, t c WHERE a.id < 4 AND b.id < 3 AND c.id = a.id + b.id"),
        json!([[6]])
    );
    assert_eq!(
        db.rows("SELECT count(*) FROM t a, t b WHERE abs(a.id) < 11 AND abs(b.id) < 11 AND a.g = b.g"),
        json!([[16]])
    );
    assert_eq!(db.rows("SELECT a.id, b.id FROM t a, t b WHERE a.id = b.id + 1499 ORDER BY 1"), json!([[1500, 1]]));
    assert_eq!(
        db.rows("SELECT count(*) FROM t a LEFT JOIN t b ON b.id = a.id + 1495 WHERE b.id IS NULL"),
        json!([[1495]])
    );
    // A condition that fails on a row WHERE refuses anyway does not fail the statement.
    assert_eq!(
        db.rows("SELECT count(*) FROM t a, t b WHERE b.g <> 0 AND a.id < 3 AND b.id < 8 AND a.id / b.g > 0"),
        json!([[3]])
    );
    assert!(
        db.err("SELECT count(*) FROM t a, t b WHERE a.id < 3 AND b.id < 8 AND a.id / b.g > 0")
            .contains("division by zero")
    );
}
