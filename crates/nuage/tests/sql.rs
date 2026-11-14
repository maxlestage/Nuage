//! SQL on Nuage DB, end to end (`Embedded::sql`, the same code as `POST
//! /v1/sql`): definitions, writes, and every kind of `SELECT`.

use memoire_orm::Value;
use nuage::{Embedded, SqlResult};
use serde_json::{Value as Json, json};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nuage-sql-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

struct Db(Embedded, std::path::PathBuf);

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

impl Db {
    fn open(name: &str) -> Db {
        let dir = temp_dir(name);
        Db(Embedded::open(&dir).unwrap(), dir)
    }

    fn run(&self, sql: &str, params: &[Json]) -> Vec<SqlResult> {
        self.0.sql(sql, params).unwrap_or_else(|e| panic!("{sql}\n→ {e}"))
    }

    fn one(&self, sql: &str, params: &[Json]) -> SqlResult {
        self.run(sql, params).pop().unwrap()
    }

    /// Rows as JSON arrays, to compare at a glance.
    fn rows(&self, sql: &str, params: &[Json]) -> Json {
        let r = self.one(sql, params);
        Json::Array(r.rows.iter().map(|row| Json::Array(row.iter().map(nuage::sql::value_to_json).collect())).collect())
    }

    fn err(&self, sql: &str) -> String {
        self.0.sql(sql, &[]).expect_err(sql).to_string()
    }
}

fn blog() -> Db {
    let db = Db::open(&format!("blog-{}", std::thread::current().name().unwrap_or("t").replace("::", "-")));
    db.run(
        "CREATE TABLE users (id SERIAL PRIMARY KEY, email VARCHAR(120) NOT NULL UNIQUE, name TEXT NOT NULL, age INT, city TEXT, \
           created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP);
         CREATE TABLE posts (id BIGINT PRIMARY KEY AUTOINCREMENT, user_id BIGINT NOT NULL REFERENCES users(id), \
           title TEXT NOT NULL, views INT NOT NULL DEFAULT 0, published BOOLEAN DEFAULT TRUE, tags JSON);
         CREATE INDEX posts_user ON posts (user_id);
         INSERT INTO users (email, name, age, city) VALUES
           ('ada@x.dev', 'Ada', 36, 'Paris'), ('bob@x.dev', 'Bob', 17, 'Lyon'),
           ('cy@x.dev', 'Cy', 52, 'Paris'), ('dee@x.dev', 'Dee', NULL, NULL);
         INSERT INTO posts (user_id, title, views, published, tags) VALUES
           (1, 'Hello', 10, true, '{\"t\": [\"intro\"]}'), (1, 'Rust', 250, true, NULL),
           (2, 'Skate', 5, false, NULL), (3, 'Été à Paris', 120, true, '{\"t\": [\"voyage\", \"paris\"]}');",
        &[],
    );
    db
}

#[test]
fn definitions_and_writes() {
    let db = blog();
    // Generated keys, defaults, RETURNING.
    let r = db.one(
        "INSERT INTO posts (user_id, title) VALUES (?, ?) RETURNING id, views, published",
        &[json!(4), json!("Premier")],
    );
    assert_eq!(
        (r.columns.clone(), r.rows[0].clone()),
        (vec!["id".into(), "views".into(), "published".into()], vec![Value::I64(5), Value::I32(0), Value::Bool(true)])
    );
    assert_eq!(r.last_insert_id, Some(Value::I64(5)));
    let r = db.one("INSERT INTO posts (user_id, title) VALUES ($1, $2)", &[json!(4), json!("Second")]);
    assert_eq!((r.rows_affected, r.last_insert_id), (1, Some(Value::I64(6))));
    assert!(db.err("INSERT INTO posts (user_id) VALUES (1)").contains("title"));
    assert!(db.err("INSERT INTO users (email, name) VALUES ('ada@x.dev', 'Twin')").to_lowercase().contains("unique"));

    // UPDATE with expressions, RETURNING the new values.
    let r = db.one(
        "UPDATE posts SET views = views * 2 + 1, title = upper(title) WHERE user_id = 1 RETURNING id, title, views",
        &[],
    );
    assert_eq!(r.rows_affected, 2);
    assert_eq!(r.rows[0], vec![Value::I64(1), Value::Text("HELLO".into()), Value::I32(21)]);
    // Constant values: one engine statement.
    assert_eq!(
        db.one("UPDATE posts SET published = ? WHERE id IN (?, ?)", &[json!(false), json!(1), json!(2)]).rows_affected,
        2
    );
    assert_eq!(db.rows("SELECT count(*) FROM posts WHERE published = false", &[]), json!([[3]]));

    // Upserts.
    db.run("INSERT INTO users (email, name, age) VALUES ('ada@x.dev', 'Ada L.', 37) ON CONFLICT (email) DO UPDATE SET name = excluded.name, age = excluded.age", &[]);
    assert_eq!(db.rows("SELECT name, age FROM users WHERE email = 'ada@x.dev'", &[]), json!([["Ada L.", 37]]));
    let r = db.one("INSERT OR IGNORE INTO users (email, name) VALUES ('bob@x.dev', 'Nope'), ('eve@x.dev', 'Eve')", &[]);
    assert_eq!(r.rows_affected, 1);
    db.run("INSERT INTO users (email, name) VALUES ('eve@x.dev', 'Eve 2') ON CONFLICT DO NOTHING", &[]);
    assert_eq!(db.rows("SELECT name FROM users WHERE email = 'eve@x.dev'", &[]), json!([["Eve"]]));
    db.run("REPLACE INTO users (id, email, name) VALUES (2, 'bob@x.dev', 'Robert')", &[]);
    assert_eq!(db.rows("SELECT name, age FROM users WHERE id = 2", &[]), json!([["Robert", null]]));

    // DELETE ... RETURNING, with a subquery (Robert, replaced, has no city any more).
    let r = db.one("DELETE FROM posts WHERE user_id IN (SELECT id FROM users WHERE city IS NULL) RETURNING title", &[]);
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Text("Skate".into())],
            vec![Value::Text("Premier".into())],
            vec![Value::Text("Second".into())]
        ]
    );
    assert_eq!(r.rows_affected, 3);

    // Indexes on existing tables, unique ones checked against the rows there.
    db.run("CREATE UNIQUE INDEX users_name ON users (name)", &[]);
    assert!(db.err("INSERT INTO users (email, name) VALUES ('z@x.dev', 'Eve')").to_lowercase().contains("unique"));
    db.run("DROP INDEX users_name", &[]);
    db.run("INSERT INTO users (email, name) VALUES ('z@x.dev', 'Eve')", &[]);
    assert!(db.err("CREATE UNIQUE INDEX users_name2 ON users (name)").to_lowercase().contains("unique"));
    db.run("CREATE INDEX IF NOT EXISTS posts_user ON posts (user_id)", &[]);
    assert_eq!(db.one("EXPLAIN SELECT * FROM posts WHERE user_id = 1", &[]).columns, vec!["plan".to_string()]);

    // New columns: the rows there get the default.
    db.run("ALTER TABLE users ADD COLUMN score DOUBLE PRECISION NOT NULL DEFAULT 1.5", &[]);
    db.run("ALTER TABLE users ADD COLUMN nick TEXT UNIQUE", &[]);
    assert_eq!(db.rows("SELECT score, nick FROM users WHERE id = 1", &[]), json!([[1.5, null]]));
    db.run("UPDATE users SET nick = 'ada' WHERE id = 1", &[]);
    assert!(db.err("UPDATE users SET nick = 'ada' WHERE id = 3").to_lowercase().contains("unique"));
    assert!(db.err("ALTER TABLE users ADD COLUMN must TEXT NOT NULL").contains("DEFAULT"));

    // A table without a key, and one made from a query.
    db.run("CREATE TABLE logs (msg TEXT, at TIMESTAMP DEFAULT now())", &[]);
    db.run("INSERT INTO logs (msg) VALUES ('a'), ('b'); INSERT INTO logs VALUES ('c', '2026-11-13 10:00:00')", &[]);
    assert_eq!(db.rows("SELECT msg FROM logs ORDER BY rowid", &[]), json!([["a"], ["b"], ["c"]]));
    db.run("CREATE TABLE paris AS SELECT id, name FROM users WHERE city = 'Paris'", &[]);
    assert_eq!(db.rows("SELECT name FROM paris ORDER BY id", &[]), json!([["Ada L."], ["Cy"]]));
    db.run("DROP TABLE IF EXISTS paris, nothing", &[]);
    assert!(db.err("SELECT * FROM paris").contains("paris"));
}

#[test]
fn one_transaction_per_request() {
    let db = blog();
    // A failure undoes the statements before it.
    let err =
        db.0.sql("INSERT INTO users (email, name) VALUES ('new@x.dev', 'New'); INSERT INTO nowhere VALUES (1)", &[]);
    assert!(err.is_err());
    assert_eq!(db.rows("SELECT count(*) FROM users WHERE email = 'new@x.dev'", &[]), json!([[0]]));
    // A table created and filled in one request.
    let all = db.run(
        "BEGIN; CREATE TABLE t (k TEXT PRIMARY KEY, n INT); INSERT INTO t VALUES ('a', 1); SELECT * FROM t; COMMIT",
        &[],
    );
    assert_eq!(all.len(), 5);
    assert_eq!(all[3].rows, vec![vec![Value::Text("a".into()), Value::I32(1)]]);
    // Parameters: the number must match.
    assert!(db.0.sql("SELECT * FROM users WHERE id = ?", &[]).unwrap_err().to_string().contains("parameter"));
    assert!(db.0.sql("SELECT * FROM users", &[json!(1)]).is_err());
}

#[test]
fn queries() {
    let db = blog();
    let q = |sql: &str| db.rows(sql, &[]);
    // Filters, order, limits, NULLs.
    assert_eq!(q("SELECT name FROM users WHERE age >= 18 ORDER BY age DESC"), json!([["Cy"], ["Ada"]]));
    assert_eq!(q("SELECT name FROM users ORDER BY age NULLS FIRST LIMIT 2"), json!([["Dee"], ["Bob"]]));
    assert_eq!(q("SELECT name FROM users ORDER BY age LIMIT 2 OFFSET 1"), json!([["Ada"], ["Cy"]]));
    assert_eq!(q("SELECT name FROM users ORDER BY id LIMIT 1, 2"), json!([["Bob"], ["Cy"]]));
    assert_eq!(q("SELECT name FROM users WHERE city IS NULL OR age < 18 ORDER BY 1"), json!([["Bob"], ["Dee"]]));
    assert_eq!(q("SELECT count(*) FROM users WHERE age > 100"), json!([[0]]));
    assert_eq!(q("SELECT name FROM users WHERE name ILIKE 'a%' OR name LIKE '_y'"), json!([["Ada"], ["Cy"]]));
    assert_eq!(q("SELECT name FROM users WHERE age NOT BETWEEN 18 AND 40 ORDER BY name"), json!([["Bob"], ["Cy"]]));
    assert_eq!(q("SELECT name FROM users WHERE id IN (1, 3) AND city IN ('Paris')"), json!([["Ada"], ["Cy"]]));
    assert_eq!(q("SELECT DISTINCT city FROM users WHERE city IS NOT NULL ORDER BY city"), json!([["Lyon"], ["Paris"]]));
    // Expressions, functions, CASE, CAST.
    assert_eq!(
        q(
            "SELECT upper(name) || ' (' || coalesce(city, '?') || ')' AS who, CASE WHEN age >= 18 THEN 'adult' WHEN age IS NULL THEN 'unknown' ELSE 'minor' END AS kind, \
           age % 10, CAST(age AS TEXT), length(email) FROM users ORDER BY id"
        ),
        json!([
            ["ADA (Paris)", "adult", 6, "36", 9],
            ["BOB (Lyon)", "minor", 7, "17", 9],
            ["CY (Paris)", "adult", 2, "52", 8],
            ["DEE (?)", "unknown", null, null, 9]
        ])
    );
    assert_eq!(
        q("SELECT 1 + 2 * 3, 7 / 2, 7.0 / 2, -(4), 'a' || 1, round(2.567, 1), abs(-3), nullif(1, 1)"),
        json!([[7, 3, 3.5, -4, "a1", 2.6, 3, null]])
    );
    assert_eq!(
        q("SELECT json_extract(tags, '$.t[0]') FROM posts WHERE tags IS NOT NULL ORDER BY id"),
        json!([["intro"], ["voyage"]])
    );
    assert_eq!(
        q("SELECT name FROM users WHERE created_at > '2020-01-01' AND created_at <= now() ORDER BY id LIMIT 1"),
        json!([["Ada"]])
    );
    // Joins.
    assert_eq!(
        q(
            "SELECT u.name, p.title FROM posts p JOIN users u ON u.id = p.user_id WHERE p.views > 6 ORDER BY p.views DESC"
        ),
        json!([["Ada", "Rust"], ["Cy", "Été à Paris"], ["Ada", "Hello"]])
    );
    assert_eq!(
        q(
            "SELECT u.name, count(p.id) AS n, coalesce(sum(p.views), 0) AS views FROM users u LEFT JOIN posts p ON p.user_id = u.id \
           GROUP BY u.id, u.name ORDER BY views DESC, u.name"
        ),
        json!([["Ada", 2, 260], ["Cy", 1, 120], ["Bob", 1, 5], ["Dee", 0, 0]])
    );
    assert_eq!(q("SELECT count(*) FROM users CROSS JOIN posts"), json!([[16]]));
    assert_eq!(
        q("SELECT count(*) FROM users, posts WHERE posts.user_id = users.id AND users.city = 'Paris'"),
        json!([[3]])
    );
    assert_eq!(q("SELECT title FROM posts JOIN users USING (id) WHERE users.name = 'Bob'"), json!([["Rust"]]));
    // Grouping.
    assert_eq!(
        q(
            "SELECT city, count(*) AS n, avg(age), min(name), max(age) FROM users GROUP BY city HAVING count(*) >= 1 ORDER BY n DESC, city NULLS LAST"
        ),
        json!([["Paris", 2, 44.0, "Ada", 52], ["Lyon", 1, 17.0, "Bob", 17], [null, 1, null, "Dee", null]])
    );
    assert_eq!(q("SELECT group_concat(name, '|') FROM users WHERE city = 'Paris'"), json!([["Ada|Cy"]]));
    assert_eq!(q("SELECT count(DISTINCT city), count(city), count(*) FROM users"), json!([[2, 3, 4]]));
    assert_eq!(
        q("SELECT published, sum(views) FROM posts GROUP BY published ORDER BY published"),
        json!([[false, 5], [true, 380]])
    );
    // Subqueries.
    assert_eq!(
        q("SELECT name FROM users WHERE id IN (SELECT user_id FROM posts WHERE views > 100) ORDER BY name"),
        json!([["Ada"], ["Cy"]])
    );
    assert_eq!(
        q("SELECT name FROM users u WHERE NOT EXISTS (SELECT 1 FROM posts p WHERE p.user_id = u.id)"),
        json!([["Dee"]])
    );
    assert_eq!(
        q(
            "SELECT name, (SELECT max(views) FROM posts p WHERE p.user_id = u.id) AS best FROM users u ORDER BY best DESC NULLS LAST, name"
        ),
        json!([["Ada", 250], ["Cy", 120], ["Bob", 5], ["Dee", null]])
    );
    assert_eq!(
        q("SELECT title FROM posts WHERE views > (SELECT avg(views) FROM posts)"),
        json!([["Rust"], ["Été à Paris"]])
    );
    assert_eq!(
        q(
            "SELECT t.city, t.n FROM (SELECT city, count(*) AS n FROM users WHERE city IS NOT NULL GROUP BY city) AS t WHERE t.n > 1"
        ),
        json!([["Paris", 2]])
    );
    // Compounds.
    assert_eq!(
        q("SELECT name FROM users WHERE city = 'Paris' UNION SELECT name FROM users WHERE age < 40 ORDER BY 1"),
        json!([["Ada"], ["Bob"], ["Cy"]])
    );
    assert_eq!(
        q("SELECT city FROM users UNION ALL SELECT 'Nice' ORDER BY city LIMIT 3"),
        json!([["Lyon"], ["Nice"], ["Paris"]])
    );
    assert_eq!(q("SELECT id FROM users INTERSECT SELECT user_id FROM posts ORDER BY 1"), json!([[1], [2], [3]]));
    assert_eq!(q("SELECT id FROM users EXCEPT SELECT user_id FROM posts"), json!([[4]]));
    // Column names: aliases, then expressions, then repeated names qualified.
    let r = db.one("SELECT u.id, p.id, name AS who, count(*) FROM users u JOIN posts p ON p.user_id = u.id GROUP BY u.id, p.id, name", &[]);
    assert_eq!(r.columns, vec!["id", "p.id", "who", "count"]);
    // Parameters in every place.
    assert_eq!(
        db.rows(
            "SELECT name FROM users WHERE age > ? AND city = ?2 ORDER BY name LIMIT ?3",
            &[json!(18), json!("Paris"), json!(1)]
        ),
        json!([["Ada"]])
    );
    assert_eq!(db.rows("SELECT ? || '!', ?1", &[json!("hé")]), json!([["hé!", "hé"]]));
    // Names in any case.
    assert_eq!(q("select NAME from USERS where ID = 1"), json!([["Ada"]]));
    // Errors say what is wrong.
    assert!(db.err("SELECT nope FROM users").contains("nope"));
    assert!(db.err("SELECT id FROM users JOIN posts ON posts.user_id = users.id").contains("ambiguous"));
    assert!(db.err("SELECT * FROM users WHERE count(*) > 1").contains("aggregate"));
    assert!(db.err("SELECT 1/0").contains("division by zero"));
    assert!(db.err("SELECT * FROM users RIGHT JOIN posts ON 1 = 1").contains("RIGHT"));
}

#[test]
fn fast_paths_and_big_tables() {
    let db = Db::open("big");
    db.run("CREATE TABLE items (id INTEGER PRIMARY KEY, grp INT INDEX, name TEXT NOT NULL, price DOUBLE)", &[]);
    for chunk in 0..10 {
        let values: Vec<String> = (0..500)
            .map(|i| {
                let n = chunk * 500 + i;
                format!("({n}, {}, 'item {n}', {})", n % 25, f64::from(n) / 10.0)
            })
            .collect();
        db.run(&format!("INSERT INTO items (id, grp, name, price) VALUES {}", values.join(",")), &[]);
    }
    assert_eq!(db.rows("SELECT count(*) FROM items", &[]), json!([[5000]]));
    // Through an index, ordered by the key, a page.
    assert_eq!(
        db.rows("SELECT id FROM items WHERE grp = ? ORDER BY id DESC LIMIT 3", &[json!(7)]),
        json!([[4982], [4957], [4932]])
    );
    // A function in the projection: tables read with the filter, early stop kept.
    assert_eq!(
        db.rows("SELECT upper(name) FROM items WHERE grp = 3 ORDER BY id LIMIT 2 OFFSET 1", &[]),
        json!([["ITEM 28"], ["ITEM 53"]])
    );
    // A parameter compared with an integer key, as text: still found.
    assert_eq!(db.rows("SELECT name FROM items WHERE id = ?", &[json!("42")]), json!([["item 42"]]));
    assert_eq!(db.rows("SELECT name FROM items WHERE id = 42.0", &[]), json!([["item 42"]]));
    // A self join through a hash join.
    assert_eq!(
        db.rows("SELECT count(*) FROM items a JOIN items b ON b.id = a.id + 1 WHERE a.grp = 0", &[]),
        json!([[200]])
    );
    assert_eq!(
        db.rows("SELECT grp, count(*), round(sum(price), 1) FROM items GROUP BY grp ORDER BY grp LIMIT 2", &[]),
        json!([[0, 200, 49750.0], [1, 200, 49770.0]])
    );
    let started = std::time::Instant::now();
    for i in 0..200 {
        db.rows("SELECT id, name FROM items WHERE grp = ? ORDER BY id DESC LIMIT 20", &[json!(i % 25)]);
    }
    let each = started.elapsed() / 200;
    assert!(each < std::time::Duration::from_millis(50), "{each:?} per page");
}

/// Ce que l'API de Seriously envoie, écrit pour PostgreSQL : verrous de ligne
/// et consultatifs, liste d'identifiants passée en un seul paramètre JSON.
#[test]
fn postgres_idioms() {
    let db = blog();
    // `FOR UPDATE` et ses variantes : acceptés, les lignes lues telles quelles.
    for lock in
        ["FOR UPDATE", "FOR UPDATE SKIP LOCKED", "FOR NO KEY UPDATE OF users NOWAIT", "FOR SHARE", "FOR KEY SHARE"]
    {
        assert_eq!(db.rows(&format!("SELECT id FROM users WHERE id = $1 {lock}"), &[json!(2)]), json!([[2]]), "{lock}");
    }
    assert_eq!(db.rows("SELECT id FROM users ORDER BY id LIMIT 1 FOR UPDATE", &[]), json!([[1]]));
    db.err("SELECT id FROM users FOR nothing");
    // La liste d'exclusion de la découverte : un tableau JSON en un paramètre.
    let sql =
        "SELECT name FROM users WHERE id NOT IN (SELECT jsonb_array_elements_text($1::jsonb)::bigint) ORDER BY id";
    assert_eq!(db.rows(sql, &[json!("[1,3]")]), json!([["Bob"], ["Dee"]]));
    assert_eq!(db.rows(sql, &[json!("[]")]), json!([["Ada"], ["Bob"], ["Cy"], ["Dee"]]));
    assert_eq!(
        db.rows(
            "SELECT name FROM users WHERE id IN (SELECT jsonb_array_elements_text($1)::bigint + 1)",
            &[json!("[0]")]
        ),
        json!([["Ada"]])
    );
    assert_eq!(db.rows("SELECT json_array_elements_text('[\"a\", 2, null]')", &[]), json!([["a"], ["2"], [null]]));
    assert_eq!(db.rows("SELECT jsonb_array_elements('[1, 2]') AS n", &[]), json!([[1], [2]]));
    assert!(db.err("SELECT jsonb_array_elements_text('{\"a\": 1}')").contains("not an array"));
    assert!(db.err("SELECT upper(jsonb_array_elements_text('[\"a\"]')) FROM users").contains("set of rows"));
    // Verrous consultatifs : rien à attendre (un commit concurrent sur une table
    // lue fait échouer la transaction), et une clé de hachage stable.
    assert_eq!(
        db.rows("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))", &[json!("report:1:2")]),
        json!([[null]])
    );
    assert_eq!(db.rows("SELECT pg_try_advisory_lock(42), pg_advisory_unlock(42)", &[]), json!([[true, true]]));
    assert_eq!(
        db.rows("SELECT hashtextextended('a', 0) = hashtextextended('a', 0), hashtextextended('a', 0) = hashtextextended('a', 1)", &[]),
        json!([[true, false]])
    );

    // Dates : INTERVAL, extract(... FROM ...), littéraux typés, agrégats booléens.
    assert_eq!(db.rows("SELECT (DATE '2026-01-31' + INTERVAL '1 month')::date", &[]), json!([["2026-02-28"]]));
    assert_eq!(
        db.rows("SELECT (TIMESTAMP '2026-03-01 10:00:00' - INTERVAL '1 day 2 hours')::text", &[]),
        json!([["2026-02-28 08:00:00"]])
    );
    assert_eq!(db.rows("SELECT INTERVAL '2 years' + DATE '2024-02-29' < DATE '2026-03-01'", &[]), json!([[true]]));
    assert_eq!(
        db.rows("SELECT now() - INTERVAL '1 hour' < now(), now() + INTERVAL '90 minutes' > now()", &[]),
        json!([[true, true]])
    );
    assert_eq!(
        db.rows(
            "SELECT extract(epoch FROM TIMESTAMP '2026-01-01 00:00:00'), extract(year FROM DATE '2026-05-04')",
            &[]
        ),
        json!([[1767225600, 2026]])
    );
    assert!(db.err("SELECT now() - INTERVAL 'bientôt'").contains("INTERVAL"));
    assert_eq!(
        db.rows("SELECT bool_and(age > 16), bool_or(age > 50), every(age > 40) FROM users", &[]),
        json!([[true, true, false]])
    );
    assert_eq!(db.rows("SELECT bool_and(age > 0) FROM users WHERE id > 100", &[]), json!([[null]]));
}

/// `EmbeddedTransaction::sql`: a transaction sees its own writes, others see
/// nothing before the commit, and a rollback undoes everything.
#[test]
fn sql_in_a_transaction() {
    let db = Db::open("transaction");
    db.0.sql("CREATE TABLE notes (id TEXT PRIMARY KEY, n BIGINT NOT NULL)", &[]).unwrap();
    let count = |rows: Vec<SqlResult>| rows[0].rows[0][0].as_i64().unwrap();

    let tx = db.0.begin_now().unwrap();
    tx.sql(
        "INSERT INTO notes (id, n) VALUES (?, ?); INSERT INTO notes (id, n) VALUES (?, ?)",
        &[json!("a"), json!(1), json!("b"), json!(2)],
    )
    .unwrap();
    assert_eq!(count(tx.sql("SELECT count(*) FROM notes", &[]).unwrap()), 2);
    assert_eq!(count(db.0.sql("SELECT count(*) FROM notes", &[]).unwrap()), 0, "not committed yet");
    tx.sql("UPDATE notes SET n = n + 10 WHERE id = ?", &[json!("a")]).unwrap();
    tx.commit_now().unwrap();
    assert_eq!(count(db.0.sql("SELECT sum(n) FROM notes", &[]).unwrap()), 13);

    let tx = db.0.begin_now().unwrap();
    tx.sql("DELETE FROM notes", &[]).unwrap();
    assert_eq!(count(tx.sql("SELECT count(*) FROM notes", &[]).unwrap()), 0);
    // A failing statement: the caller rolls back.
    assert!(tx.sql("INSERT INTO notes (id, n) VALUES (?, ?)", &[json!("c")]).is_err());
    tx.rollback_now().unwrap();
    assert_eq!(count(db.0.sql("SELECT count(*) FROM notes", &[]).unwrap()), 2);

    // Dropped without a commit: nothing written.
    {
        let tx = db.0.begin_now().unwrap();
        tx.sql("INSERT INTO notes (id, n) VALUES ('d', 4)", &[]).unwrap();
    }
    assert_eq!(count(db.0.sql("SELECT count(*) FROM notes", &[]).unwrap()), 2);
}

/// `COUNT(*)` answered from an index (equalities that pin exactly its columns)
/// gives the same count as the rows themselves: integers, text, NULL, a
/// literal of another type, a two-column index, and the uncommitted writes of
/// a transaction.
#[test]
fn count_from_an_index_matches_the_rows() {
    let db = Db::open("count-index");
    db.0.sql(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER, s TEXT, a INTEGER, b TEXT);
         CREATE INDEX t_n ON t (n);
         CREATE INDEX t_s ON t (s);
         CREATE INDEX t_ab ON t (a, b)",
        &[],
    )
    .unwrap();
    for i in 0..300i64 {
        let n = if i % 11 == 0 { Json::Null } else { json!(i % 7) };
        let s = if i % 13 == 0 { Json::Null } else { json!(format!("v{}", i % 5)) };
        db.0.sql(
            "INSERT INTO t (id, n, s, a, b) VALUES (?, ?, ?, ?, ?)",
            &[json!(i), n, s, json!(i % 3), json!(format!("b{}", i % 4))],
        )
        .unwrap();
    }
    db.0.sql("UPDATE t SET n = 99 WHERE id < 20; DELETE FROM t WHERE id % 17 = 0", &[]).unwrap();
    let filters: Vec<(&str, Vec<Json>)> = vec![
        ("n = ?", vec![json!(3)]),
        ("n = ?", vec![json!(99)]),
        ("n = ?", vec![json!(1234)]),
        ("n = ?", vec![Json::Null]),
        ("n = ?", vec![json!("3")]),
        ("n = ?", vec![json!(3.0)]),
        ("? = n", vec![json!(4)]),
        ("s = ?", vec![json!("v2")]),
        ("s = ?", vec![json!("V2")]),
        ("a = ? AND b = ?", vec![json!(1), json!("b2")]),
        ("b = ? AND a = ?", vec![json!("b3"), json!(0)]),
        ("a = ?", vec![json!(2)]),
        ("n = ? AND n = ?", vec![json!(3), json!(3)]),
        ("n = ? AND n = ?", vec![json!(3), json!(4)]),
        ("n = ? AND s = ?", vec![json!(3), json!("v3")]),
    ];
    let check = |run: &dyn Fn(&str, &[Json]) -> Vec<SqlResult>, when: &str| {
        for (filter, params) in &filters {
            let count = run(&format!("SELECT count(*) FROM t WHERE {filter}"), params)[0].rows[0][0].as_i64().unwrap();
            let rows = run(&format!("SELECT id FROM t WHERE {filter}"), params)[0].rows.len() as i64;
            assert_eq!(count, rows, "{when} : {filter} {params:?}");
        }
    };
    check(&|sql, p| db.0.sql(sql, p).unwrap(), "base");
    // In a transaction: its own writes count, and nothing else sees them.
    let tx = db.0.begin_now().unwrap();
    tx.sql(
        "INSERT INTO t (id, n, s, a, b) VALUES (1000, 3, 'v2', 1, 'b2'); \
         UPDATE t SET n = 4, s = 'v2' WHERE id = 30; DELETE FROM t WHERE n = 5",
        &[],
    )
    .unwrap();
    check(&|sql, p| tx.sql(sql, p).unwrap(), "transaction");
    tx.rollback_now().unwrap();
    check(&|sql, p| db.0.sql(sql, p).unwrap(), "après annulation");
}

/// A `SELECT` of plain columns found by a key or an index is answered from the
/// rows read, without the evaluator. `AND 1 = 1` sends the same statement
/// through it: both give the same rows, in the same order.
#[test]
fn answers_without_the_evaluator_match_it() {
    let db = Db::open("direct");
    db.run(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, code TEXT UNIQUE, grp INT, kind TEXT, score DOUBLE PRECISION, note TEXT);
         CREATE INDEX t_grp_kind ON t (grp, kind);
         CREATE INDEX t_score ON t (score);
         CREATE TABLE named (name TEXT PRIMARY KEY, n INT)",
        &[],
    );
    for i in 0..60i64 {
        db.run(
            "INSERT INTO t (id, code, grp, kind, score, note) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                json!(i),
                json!(format!("c{i:02}")),
                if i % 11 == 0 { Json::Null } else { json!(i % 4) },
                json!(["a", "b", "c"][(i % 3) as usize]),
                json!(i as f64 / 4.0),
                if i % 7 == 0 { Json::Null } else { json!(format!("note {i}")) },
            ],
        );
    }
    db.run("INSERT INTO named (name, n) VALUES ('5', 1), ('abc', 2), ('', 3)", &[]);
    let statements: &[(&str, Vec<Json>)] = &[
        ("SELECT note FROM t WHERE id = ?", vec![json!(5)]),
        ("SELECT * FROM t WHERE id = ?", vec![json!(5)]),
        ("SELECT * FROM t WHERE id = ?", vec![json!(999)]),
        ("SELECT id, note, id FROM t WHERE id = ?", vec![json!(8)]),
        ("SELECT score, code FROM t WHERE id = ?", vec![json!(7.0)]),
        ("SELECT id FROM t WHERE id = ?", vec![json!("5")]),
        ("SELECT id FROM t WHERE id = ?", vec![Json::Null]),
        ("SELECT id FROM t WHERE id = NULL", vec![]),
        ("SELECT id, kind FROM t WHERE id IN (?, ?, ?, ?)", vec![json!(9), json!(3), json!(3), json!(1000)]),
        ("SELECT id FROM t WHERE id IN (4, NULL, 2)", vec![]),
        // Long lists, of numbers written both ways.
        ("SELECT id, code FROM t WHERE id IN (40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 1000, 7)", vec![]),
        ("SELECT id FROM t WHERE id IN (1, 2.0, 3, 4, 5, 6.0, 7, 8, 9, 10.5, 11, 12)", vec![]),
        ("SELECT id FROM t WHERE id IN (SELECT id FROM t WHERE score < 9)", vec![]),
        ("SELECT id, code FROM t WHERE code = ?", vec![json!("c17")]),
        ("SELECT id FROM t WHERE code IN ('c17', 'c03', 'nope')", vec![]),
        ("SELECT id, note FROM t WHERE grp = ? AND kind = ?", vec![json!(2), json!("b")]),
        ("SELECT id FROM t WHERE kind = ? AND grp = ?", vec![json!("c"), json!(1)]),
        ("SELECT id FROM t WHERE grp = ? AND kind = ?", vec![Json::Null, json!("a")]),
        ("SELECT id FROM t WHERE grp = 1 AND grp = 2 AND kind = 'a'", vec![]),
        ("SELECT id FROM t WHERE grp = 1 AND kind = 'a' AND note IS NOT NULL", vec![]),
        ("SELECT id FROM t WHERE score = ?", vec![json!(2.5)]),
        ("SELECT id FROM t WHERE score = ?", vec![json!(3)]),
        ("SELECT id FROM t WHERE grp = ? AND kind = ? LIMIT 2", vec![json!(3), json!("a")]),
        ("SELECT id FROM t WHERE grp = ? AND kind = ? LIMIT 2 OFFSET 1", vec![json!(3), json!("a")]),
        ("SELECT id FROM t WHERE grp = ? AND kind = ? LIMIT 0", vec![json!(3), json!("a")]),
        ("SELECT id FROM t WHERE grp = ? AND kind = ? ORDER BY id DESC", vec![json!(3), json!("a")]),
        ("SELECT id FROM t WHERE grp = ? AND kind = ? ORDER BY id DESC LIMIT 2", vec![json!(3), json!("a")]),
        ("SELECT id FROM t WHERE grp = ? AND kind = ? ORDER BY note LIMIT 3 OFFSET 1", vec![json!(1), json!("b")]),
        ("SELECT id FROM t WHERE id = ? ORDER BY note", vec![json!(12)]),
        ("SELECT id, score FROM t WHERE id = ? LIMIT 1", vec![json!(12)]),
        ("SELECT id, score FROM t WHERE id = ? OFFSET 1", vec![json!(12)]),
        ("SELECT n FROM named WHERE name = ?", vec![json!("5")]),
        ("SELECT n FROM named WHERE name = ?", vec![json!(5)]),
        ("SELECT n FROM named WHERE name = ?", vec![json!("")]),
        ("SELECT upper(note) FROM t WHERE id = ?", vec![json!(5)]),
        ("SELECT DISTINCT kind FROM t WHERE grp = ? AND kind = ?", vec![json!(2), json!("b")]),
    ];
    for (sql, params) in statements {
        let direct = db.rows(sql, params);
        // The same filter with a conjunct no index answers.
        let through = sql.replacen(" WHERE ", " WHERE 1 = 1 AND ", 1);
        assert_ne!(*sql, through);
        assert_eq!(direct, db.rows(&through, params), "{sql} {params:?}");
    }
    // The answers themselves, for a few.
    assert_eq!(db.rows("SELECT id, note, id FROM t WHERE id = 8", &[]), json!([[8, "note 8", 8]]));
    assert_eq!(db.rows("SELECT id FROM t WHERE id IN (9, 3, 3, 1000)", &[]), json!([[3], [9]]));
    assert_eq!(db.rows("SELECT id FROM t WHERE grp = 3 AND kind = 'a' LIMIT 2 OFFSET 1", &[]), json!([[15], [27]]));
    assert_eq!(db.rows("SELECT id FROM t WHERE id = NULL", &[]), json!([]));
    assert_eq!(db.rows("SELECT n FROM named WHERE name = ''", &[]), json!([[3]]));
}

/// Writes sent at the same time each take their turn: none is refused for a
/// conflict, and none is lost (a counter raised by every one of them ends at
/// their number).
#[test]
fn writes_at_the_same_time_take_turns() {
    let db = Db::open("turns");
    db.run("CREATE TABLE counters (name TEXT PRIMARY KEY, n BIGINT NOT NULL); CREATE TABLE log (id SERIAL PRIMARY KEY, who INT)", &[]);
    db.run("INSERT INTO counters (name, n) VALUES ('hits', 0)", &[]);
    let (threads, each) = (16, 40);
    std::thread::scope(|scope| {
        for t in 0..threads {
            let db = &db.0;
            scope.spawn(move || {
                for _ in 0..each {
                    // Read, then write what was read: lost if two of them ran on the same value.
                    db.sql(
                        "UPDATE counters SET n = n + 1 WHERE name = 'hits'; INSERT INTO log (who) VALUES (?)",
                        &[json!(t)],
                    )
                    .unwrap();
                    // And a read alone in between: it never waits for a turn.
                    db.sql("SELECT n FROM counters WHERE name = 'hits'", &[]).unwrap();
                }
            });
        }
    });
    let total = threads * each;
    assert_eq!(db.rows("SELECT n FROM counters", &[]), json!([[total]]));
    assert_eq!(db.rows("SELECT count(*), count(DISTINCT id) FROM log", &[]), json!([[total, total]]));
    // A dry run changes nothing, and a failing script leaves nothing behind.
    assert!(db.0.sql("UPDATE counters SET n = 0; INSERT INTO counters (name, n) VALUES ('hits', 1)", &[]).is_err());
    assert_eq!(db.rows("SELECT n FROM counters", &[]), json!([[total]]));
}
