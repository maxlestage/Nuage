//! Statements nested as deeply as the parser takes, and long flat lists, on a
//! thread of 2 MiB (what the threads of the runtime and the PostgreSQL
//! sessions have), unoptimized: read, run, typed, copied, printed, dropped.
//! A statement once took the stack of its thread and aborted the process.

use super::*;
use crate::embedded::Embedded;

/// A thread with the default stack of Rust and Tokio.
const STACK: usize = 2 << 20;

fn on_small_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| std::thread::Builder::new().stack_size(STACK).spawn_scoped(s, f).unwrap().join().unwrap())
}

fn nest(open: &str, core: &str, close: &str, n: usize) -> String {
    format!("{}{}{}", open.repeat(n), core, close.repeat(n))
}

fn list(sep: &str, n: usize, item: impl Fn(usize) -> String) -> String {
    (0..n).map(item).collect::<Vec<_>>().join(sep)
}

struct Db(Embedded, std::path::PathBuf);

impl Drop for Db {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn db(name: &str) -> Db {
    let dir = std::env::temp_dir().join(format!("nuage-nesting-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Embedded::open(&dir).unwrap();
    db.sql("CREATE TABLE t (id INT PRIMARY KEY, n INT, s TEXT); INSERT INTO t VALUES (1, 1, 'a'), (2, 2, 'b')", &[])
        .unwrap();
    Db(db, dir)
}

type Shape = (&'static str, fn(usize) -> String);

/// Every way of nesting: through each level of precedence, each kind of
/// subquery, the paths through Memoire's statements and through this module.
const SHAPES: &[Shape] = &[
    ("parentheses", |n| format!("SELECT {}", nest("(", "1", ")", n))),
    ("NOT", |n| format!("SELECT {} FROM t", nest("NOT ", "n", "", n))),
    ("minus", |n| format!("SELECT {} FROM t", nest("- ", "n", "", n))),
    ("functions", |n| format!("SELECT {} FROM t", nest("abs(", "n", ")", n))),
    ("CASE", |n| format!("SELECT {} FROM t", nest("CASE WHEN n > 0 THEN ", "n", " END", n))),
    ("CAST", |n| format!("SELECT {} FROM t", nest("CAST(", "n", " AS INT)", n))),
    ("::", |n| format!("SELECT n{} FROM t", "::int".repeat(n))),
    ("every level", |n| format!("SELECT {} FROM t", nest("(0 OR 0 AND NOT 0 = '' || 0 + 0 * -(", "n", ")::int)", n))),
    ("LIKE", |n| format!("SELECT {} FROM t", nest("('a' LIKE ", "s", ")", n))),
    ("BETWEEN", |n| format!("SELECT {} FROM t", nest("(1 BETWEEN 0 AND ", "n", ")", n))),
    ("IS DISTINCT FROM", |n| format!("SELECT {} FROM t", nest("(n IS DISTINCT FROM ", "n", ")", n))),
    ("IN lists", |n| format!("SELECT {} FROM t", nest("(1 IN (", "n", "))", n))),
    ("scalar subqueries", |n| format!("SELECT {} FROM t", nest("(SELECT ", "t.n", ")", n))),
    ("joined subqueries", |n| {
        format!(
            "SELECT {}",
            nest(
                "(SELECT max(a.n) + ",
                "1",
                " FROM t a JOIN t b ON abs(a.n) = b.n WHERE abs(a.n) > 0 GROUP BY a.s ORDER BY 1 LIMIT 1)",
                n
            )
        )
    }),
    ("derived tables", |n| format!("SELECT * FROM {}", nest("(SELECT abs(n) AS n FROM ", "t", ") AS x", n))),
    ("IN subqueries", |n| {
        format!("SELECT 1 FROM t WHERE {}", nest("abs(n) IN (SELECT n FROM t WHERE ", "abs(n) > 0", ")", n))
    }),
    ("EXISTS", |n| {
        format!(
            "SELECT 1 FROM t a WHERE {}",
            nest("EXISTS (SELECT 1 FROM t b WHERE b.n = a.n AND ", "abs(a.n) > 0", ")", n)
        )
    }),
    ("WHERE for Memoire", |n| format!("SELECT * FROM t WHERE {}", nest("(n = 1 AND (n = 5 OR ", "n = 1", "))", n))),
    ("HAVING", |n| format!("SELECT s FROM t GROUP BY s HAVING {}", nest("NOT ", "count(*) > 0", "", n))),
    ("JOIN ON", |n| format!("SELECT * FROM t a JOIN t b ON {}", nest("NOT ", "a.n = b.n", "", n))),
    ("UPDATE", |n| {
        format!("UPDATE t SET n = {0} WHERE {1} RETURNING {0}", nest("abs(", "n", ")", n), nest("NOT ", "n = 1", "", n))
    }),
    ("DELETE", |n| format!("DELETE FROM t WHERE id = 99 AND {} RETURNING id", nest("NOT ", "abs(n) = 1", "", n))),
    ("INSERT", |n| {
        format!(
            "INSERT INTO t (id, n) VALUES (99, {}) ON CONFLICT (id) DO UPDATE SET n = {}",
            nest("abs(", "1", ")", n),
            nest("abs(", "excluded.n", ")", n)
        )
    }),
    ("EXPLAIN", |n| format!("{}SELECT 1", "EXPLAIN ".repeat(n))),
];

/// The deepest nesting of `shape` the parser takes.
fn deepest(shape: fn(usize) -> String) -> usize {
    let (mut ok, mut no) = (0, 1);
    while Script::parse(&shape(no)).is_ok() {
        (ok, no) = (no, no * 2);
    }
    while no - ok > 1 {
        let mid = (ok + no) / 2;
        if Script::parse(&shape(mid)).is_ok() { ok = mid } else { no = mid }
    }
    ok
}

#[test]
fn the_deepest_statements_fit_a_small_stack() {
    let db = db("deepest");
    for (name, shape) in SHAPES {
        let n = deepest(*shape);
        assert!(n >= 8, "{name}: only {n} levels");
        let sql = shape(n);
        on_small_stack(|| {
            let script = Script::parse(&sql).unwrap();
            // Run, and rolled back: each shape finds the same table.
            let ran = run(db.0.engine(), "main", &db.0.engine().schemas, &[(script.clone(), Vec::new())], false);
            assert!(ran.is_ok(), "{name} ({n} levels): {:?}", ran.err());
            #[cfg(feature = "server")]
            typing(&db.0.begin_read().unwrap(), &db.0.engine().schemas, &script);
            let copy: Vec<ast::Stmt> = (*script.stmts).clone();
            assert!(copy == *script.stmts && !format!("{copy:?}").is_empty());
            drop(copy);
        });
    }
}

#[test]
fn deeper_statements_are_refused_cleanly() {
    let db = db("deeper");
    on_small_stack(|| {
        for (name, shape) in SHAPES {
            let e = db.0.sql(&shape(5_000), &[]).expect_err(name).to_string();
            assert!(e.contains("nested too deeply"), "{name}: {e}");
        }
        // A column's DEFAULT is read with the same parser.
        assert!(parse::parse_expr(&nest("(", "1", ")", 5_000)).unwrap_err().to_string().contains("nested too deeply"));
    });
}

/// Lists a query builder writes are flat, however long: they work.
#[test]
fn long_lists_work() {
    let db = db("lists");
    let count = |sql: &str| -> Json {
        let r = db.0.sql(sql, &[]).unwrap_or_else(|e| panic!("{}: {e}", &sql[..80]));
        value::to_json(&r[0].rows[0][0])
    };
    on_small_stack(|| {
        let ors = |item: &dyn Fn(usize) -> String| list(" OR ", 10_000, item);
        assert_eq!(count(&format!("SELECT count(*) FROM t WHERE {}", ors(&|i| format!("id = {i}")))), 2);
        assert_eq!(count(&format!("SELECT count(*) FROM t WHERE {}", ors(&|i| format!("abs(id) = {i}")))), 2);
        let ands = |item: &dyn Fn(usize) -> String| list(" AND ", 10_000, item);
        assert_eq!(count(&format!("SELECT count(*) FROM t WHERE {}", ands(&|i| format!("id <> {}", i + 3)))), 2);
        assert_eq!(count(&format!("SELECT count(*) FROM t WHERE {}", ands(&|i| format!("lower(s) <> 'x{i}'")))), 2);
        assert_eq!(count(&format!("SELECT {}", list("+", 10_000, |_| "1".into()))), 10_000);
        assert_eq!(count(&format!("SELECT {}", list(" - ", 10_000, |i| (i * 2).to_string()))), -99_990_000);
        assert_eq!(count(&format!("SELECT length({})", list("||", 10_000, |_| "'a'".into()))), 10_000);
        let ins = list(",", 50_000, |i| i.to_string());
        assert_eq!(count(&format!("SELECT count(*) FROM t WHERE id IN ({ins})")), 2);
        assert_eq!(count(&format!("SELECT count(*) FROM t WHERE abs(id) NOT IN ({ins})")), 0);
        let unions = list(" UNION ALL ", 2_000, |i| format!("SELECT {i}"));
        assert_eq!(count(&format!("SELECT count(*) FROM ({unions}) AS u")), 2_000);
        // Updated through a long condition, read back through another.
        db.0.sql(&format!("UPDATE t SET n = 7 WHERE {}", ors(&|i| format!("abs(id) = {}", i + 2))), &[]).unwrap();
        assert_eq!(count(&format!("SELECT sum(n) FROM t WHERE {}", ands(&|i| format!("n <> {}", i + 8)))), 8);
    });
}
