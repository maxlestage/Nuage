//! The memory and the time a statement may take: settings of the process
//! (`nuage::sql::set_statement_memory`, `set_statement_timeout`), so tried in
//! a test binary of their own, in one test that changes them in turn.

use std::time::{Duration, Instant};

use nuage::Embedded;
use nuage::sql::{set_statement_memory, set_statement_timeout, statement_memory, statement_timeout};
use serde_json::{Value as Json, json};

struct Db(Embedded, std::path::PathBuf);

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

impl Db {
    fn open(name: &str) -> Db {
        let dir = std::env::temp_dir().join(format!("nuage-sql-limits-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Db(Embedded::open(&dir).unwrap(), dir)
    }

    fn rows(&self, sql: &str) -> Json {
        let r = self.0.sql(sql, &[]).unwrap_or_else(|e| panic!("{sql}\n→ {e}")).pop().unwrap();
        Json::Array(r.rows.iter().map(|row| Json::Array(row.iter().map(nuage::sql::value_to_json).collect())).collect())
    }

    fn err(&self, sql: &str) -> String {
        self.0.sql(sql, &[]).expect_err(sql).to_string()
    }
}

#[test]
fn a_statement_has_a_memory_budget_and_a_deadline() {
    // Without a call: an eighth of the memory, a minute.
    let default = statement_memory();
    assert!(((32 << 20)..=(2 << 30)).contains(&default), "{default}");
    assert_eq!(statement_timeout(), Some(Duration::from_secs(60)));

    let db = Db::open("budget");
    let rows: Vec<String> = (1..=3000).map(|i| format!("({i}, {}, 'row {i}', 1)", i % 1000)).collect();
    let wide: Vec<String> = (1..=3000).map(|i| format!("({i}, repeat('x', 2000))")).collect();
    db.0.sql(
        &format!(
            "CREATE TABLE t (id INT PRIMARY KEY, n INT, s TEXT, g INT); INSERT INTO t VALUES {};
             CREATE TABLE u (id INT PRIMARY KEY, k INT); INSERT INTO u SELECT id, 1 FROM t;
             CREATE TABLE wide (id INT PRIMARY KEY, s TEXT); INSERT INTO wide VALUES {}",
            rows.join(","),
            wide.join(",")
        ),
        &[],
    )
    .unwrap();

    set_statement_memory(4 << 20);
    assert_eq!(statement_memory(), 4 << 20);
    let over = |sql: &str| {
        let e = db.err(sql);
        assert!(e.contains("more than 4 MiB") && e.contains("LIMIT") && e.contains("NUAGE_STATEMENT_MB"), "{sql}: {e}");
    };
    // What fits does not change: a product counted, not built; filtered as it is built;
    // joined on an equality of WHERE.
    assert_eq!(db.rows("SELECT count(*) FROM t a CROSS JOIN t b"), json!([[9_000_000]]));
    assert_eq!(db.rows("SELECT count(*) FROM t a, t b WHERE a.id < 10 AND b.id < 10"), json!([[81]]));
    assert_eq!(db.rows("SELECT count(*) FROM t a, t b WHERE a.id = b.n"), json!([[2997]]));
    assert_eq!(db.rows("SELECT count(*) FROM t JOIN u ON u.k = t.g"), json!([[9_000_000]]));
    assert_eq!(db.rows("SELECT id FROM wide ORDER BY id DESC LIMIT 1"), json!([[3000]]));
    // What does not fit fails, whatever holds it.
    over("SELECT * FROM t a CROSS JOIN t b");
    over("SELECT a.id FROM t a JOIN t b ON a.n < b.n");
    over("SELECT a.s FROM t a, t b ORDER BY 1");
    over("SELECT DISTINCT a.s || b.s FROM t a, t b");
    over("SELECT t.id, u.id FROM t JOIN u ON u.k = t.g");
    over("SELECT * FROM wide");
    over("SELECT s FROM wide UNION SELECT s || 'y' FROM wide");
    over("SELECT length(string_agg(s, ',')) FROM wide");
    over("CREATE TABLE copy AS SELECT a.id AS x, b.id AS y FROM t a, t b");
    over("INSERT INTO u SELECT a.id * 10000 + b.id, 0 FROM t a, t b");
    over("UPDATE wide SET s = s || 'y'");
    over("DELETE FROM wide WHERE length(s) > 0 RETURNING s");
    // The statement's transaction is rolled back, as for any other error.
    assert!(db.0.sql("INSERT INTO t VALUES (5000, 0, 'new', 0); SELECT * FROM t a, t b", &[]).is_err());
    assert_eq!(db.rows("SELECT count(*) FROM t WHERE id = 5000"), json!([[0]]));
    assert_eq!(db.rows("SELECT count(*) FROM wide WHERE s LIKE '%y'"), json!([[0]]));
    assert!(db.err("SELECT * FROM copy").contains("copy"));
    // The statements of one request share it: their answers are held together.
    let twice = "SELECT s FROM wide WHERE id <= 1500; SELECT s FROM wide WHERE id > 1500";
    assert!(db.0.sql("SELECT s FROM wide WHERE id <= 1500", &[]).is_ok());
    assert!(db.0.sql(twice, &[]).unwrap_err().to_string().contains("NUAGE_STATEMENT_MB"));

    // 0 gives the default back.
    set_statement_memory(0);
    assert_eq!(statement_memory(), default);
    assert_eq!(db.rows("SELECT count(*) FROM (SELECT a.id FROM t a, t b WHERE a.id < 100) AS x"), json!([[297_000]]));

    // The deadline: the loops over rows read the clock.
    set_statement_timeout(Some(Duration::from_millis(200)));
    assert_eq!(statement_timeout(), Some(Duration::from_millis(200)));
    for sql in [
        "SELECT count(*) FROM t a, t b, t c WHERE a.id < 300 AND b.id < 300 AND a.n + b.n + c.n = -1",
        "SELECT count(*) FROM t a WHERE (SELECT count(*) FROM t b, t c WHERE b.n + c.n = a.n - 9999) > 0",
        // A long text against a long pattern.
        "SELECT repeat('a', 3000000) LIKE '%' || repeat('a', 3000) || 'b'",
    ] {
        let started = Instant::now();
        let e = db.err(sql);
        assert!(e.contains("statement timeout") && e.contains("NUAGE_STATEMENT_SECONDS"), "{sql}: {e}");
        assert!(started.elapsed() < Duration::from_secs(20), "{sql}: {:?}", started.elapsed());
    }
    // Each statement has its own: a request of short ones is not cut.
    let short = vec!["SELECT count(*) FROM t WHERE abs(n) >= 0"; 20].join("; ");
    assert_eq!(db.0.sql(&short, &[]).unwrap().len(), 20);
    set_statement_timeout(None);
    assert_eq!(statement_timeout(), None);
    set_statement_timeout(Some(Duration::from_secs(60)));
}
