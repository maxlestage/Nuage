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
