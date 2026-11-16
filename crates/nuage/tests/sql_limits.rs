//! The memory and the time a statement may take: settings of the process
//! (`nuage::sql::set_statement_memory`, `set_statement_timeout`), so tried in
//! a test binary of their own, in one test that changes them in turn.

use std::time::{Duration, Instant};

use memoire_orm::ast::{Join, JoinKind, Projection, SelectAst};
use memoire_orm::prelude::*;
use memoire_orm::{BinOp, ColumnRef, Expr, Query, Value};
use nuage::Embedded;
use nuage::sql::{set_statement_memory, set_statement_timeout, statement_memory, statement_timeout};
use serde_json::{Value as Json, json};

/// Two tables read through Memoire's own statements, as the wire protocol does.
#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "lefts")]
struct Left {
    #[memoire(primary_key)]
    id: i64,
    n: i64,
}

#[derive(Model, Debug, Clone, PartialEq)]
#[memoire(table = "rights")]
struct Right {
    #[memoire(primary_key)]
    id: i64,
    n: i64,
}

/// `Left` joined with `Right` on `on`, every column of both.
fn joined(on: Expr) -> SelectAst {
    let mut ast = SelectAst::new(Left::SCHEMA);
    ast.joins.push(Join { kind: JoinKind::Inner, table: Right::SCHEMA, on });
    ast.projection = Projection::Tables(vec![Left::SCHEMA, Right::SCHEMA]);
    ast
}

fn column(schema: &'static memoire_orm::TableSchema, index: usize) -> Expr {
    let c = &schema.columns[index];
    Expr::Column(ColumnRef { table: schema.name, name: c.name, index, kind: c.kind })
}

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
             CREATE TABLE wide (id INT PRIMARY KEY, s TEXT); INSERT INTO wide VALUES {};
             CREATE TABLE lefts (id BIGINT PRIMARY KEY, n BIGINT); INSERT INTO lefts SELECT id, id FROM t;
             CREATE TABLE rights (id BIGINT PRIMARY KEY, n BIGINT); INSERT INTO rights SELECT id, id % 1500 FROM t",
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
    // Only the first rows, or only whether there is one: the join stops there.
    assert_eq!(db.rows("SELECT a.id, b.id FROM t a CROSS JOIN t b LIMIT 2"), json!([[1, 1], [1, 2]]));
    assert_eq!(db.rows("SELECT EXISTS (SELECT 1 FROM t a CROSS JOIN t b)"), json!([[true]]));
    assert_eq!(db.rows("SELECT count(1) FROM t a CROSS JOIN t b"), json!([[9_000_000]]));
    assert_eq!(db.rows("SELECT length(repeat('x', 1000000) || repeat('y', 1000000))"), json!([[2_000_000]]));
    // A table larger than the budget, counted through a filter on its wide column.
    assert_eq!(db.rows("SELECT count(*), min(id) FROM wide WHERE s LIKE 'x%'"), json!([[3000, 1]]));
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
    over("DELETE FROM wide WHERE length(s) > 0 RETURNING s");
    // A table larger than the budget is changed a chunk at a time: whole, through the
    // SQL layer or the engine, and rolled back.
    let changes = "UPDATE wide SET s = s || 'y'; UPDATE wide SET s = 'z' || s WHERE abs(id) > 0;
                   UPDATE wide SET s = 'q' WHERE s LIKE 'z%'; UPDATE wide SET s = s || 'r' WHERE id > 0;
                   SELECT count(*) FROM wide WHERE s = 'qr'; DELETE FROM wide WHERE id > 0; SELECT count(*) FROM wide";
    let done = db.0.begin_now().unwrap();
    let r = done.sql(changes, &[]).unwrap();
    assert_eq!(r.iter().map(|r| r.rows_affected).collect::<Vec<_>>(), [3000, 3000, 3000, 3000, 0, 3000, 0]);
    assert_eq!((&r[4].rows, &r[6].rows), (&vec![vec![Value::I64(3000)]], &vec![vec![Value::I64(0)]]));
    done.rollback_now().unwrap();
    assert_eq!(db.rows("DELETE FROM wide WHERE id > 2999 AND abs(id) > 0 RETURNING id"), json!([[3000]]));
    assert_eq!(db.rows("SELECT count(*), max(id) FROM wide"), json!([[2999, 2999]]));
    db.0.sql("INSERT INTO wide VALUES (3000, repeat('x', 2000))", &[]).unwrap();
    // Large values held at once by one expression, one row: each fits, not all of them.
    let mb = "repeat('x', 1000000)";
    over(&format!("SELECT length(coalesce({mb}, {mb}, {mb}, {mb}, {mb}))"));
    over(&format!("SELECT 'a' IN ({mb}, {mb}, {mb}, {mb}, {mb})"));
    over(&format!("SELECT {mb} a, {mb} b, {mb} c, {mb} d, {mb} e"));
    over(&format!("SELECT length(concat({mb}, {mb}, {mb}, {mb}, {mb}))"));
    over(&format!("SELECT greatest({mb}, {mb}, {mb}, {mb}, {mb}) = 'x'"));
    over(&format!("SELECT {mb} = ({mb} = ({mb} = ({mb} = ({mb} = 'x'))))"));
    over("INSERT INTO wide VALUES (9001, repeat('x', 3000000)), (9002, repeat('x', 3000000))");
    // A text larger than the statement may hold is refused before it is built.
    over("SELECT length(repeat('x', 5000000))");
    // Memoire's own statements (the wire protocol) are held to the budget too: a join
    // that may not fit is refused, one whose pairs fit is answered.
    let refused = db.0.fetch_rows(Query::Select(&joined(Expr::Value(Value::Bool(true))))).unwrap_err().to_string();
    assert!(
        refused.contains("the join may give more rows")
            && !refused.contains("SQL")
            && refused.contains("NUAGE_STATEMENT_MB"),
        "{refused}"
    );
    let equal = joined(Expr::binary(BinOp::Eq, column(Left::SCHEMA, 1), column(Right::SCHEMA, 1)));
    assert_eq!(db.0.fetch_rows(Query::Select(&equal)).unwrap().len(), 2998);
    assert_eq!(db.0.fetch_rows(Query::Select(&SelectAst::new(Left::SCHEMA))).unwrap().len(), 3000);
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
    let long_work = vec!["length(upper(repeat('ß', 2000000)))"; 200].join(" + ");
    for sql in [
        "SELECT count(*) FROM t a, t b, t c WHERE a.id < 300 AND b.id < 300 AND a.n + b.n + c.n = -1",
        "SELECT count(*) FROM t a WHERE (SELECT count(*) FROM t b, t c WHERE b.n + c.n = a.n - 9999) > 0",
        // A long text against a long pattern.
        "SELECT repeat('a', 3000000) LIKE '%' || repeat('a', 3000) || 'b'",
        // One row, no table: the functions' work reads the clock.
        &format!("SELECT {long_work}"),
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
