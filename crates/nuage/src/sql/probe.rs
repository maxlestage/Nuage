//! TEMPORARY: stack use per level of nesting (calibration, not committed).
use super::*;

fn stack_used<R: Send>(f: impl FnOnce() -> R + Send) -> (usize, R) {
    const STACK: usize = 256 << 20;
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(STACK)
            .spawn_scoped(s, || {
                let marker = 0u8;
                let top = &marker as *const u8 as usize;
                let lo = top - (240 << 20);
                let hi = top - (64 << 10);
                unsafe {
                    let mut a = lo;
                    while a < hi {
                        std::ptr::write_volatile(a as *mut u8, 0xA5);
                        a += 1;
                    }
                }
                let r = f();
                let mut a = lo;
                unsafe {
                    while a < hi && std::ptr::read_volatile(a as *const u8) == 0xA5 {
                        a += 1;
                    }
                }
                (top - a, r)
            })
            .unwrap()
            .join()
            .unwrap()
    })
}

fn nest(open: &str, core: &str, close: &str, n: usize) -> String {
    format!("{}{}{}", open.repeat(n), core, close.repeat(n))
}

#[test]
fn probe() {
    // limits lifted for the probe: see parse.rs constants (set large by env at build)
    let dir = std::env::temp_dir().join(format!("nuage-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Embedded::open(&dir).unwrap();
    db.sql("CREATE TABLE t (id INT PRIMARY KEY, n INT, s TEXT); INSERT INTO t VALUES (1, 1, 'a'), (2, 2, 'b')", &[]).unwrap();
    let shapes: Vec<(&str, Box<dyn Fn(usize) -> String>)> = vec![
        ("parens", Box::new(|n| format!("SELECT {}", nest("(", "1", ")", n)))),
        ("parens col", Box::new(|n| format!("SELECT {} FROM t", nest("(", "n", ")", n)))),
        ("not", Box::new(|n| format!("SELECT {} FROM t", nest("NOT ", "n", "", n)))),
        ("neg", Box::new(|n| format!("SELECT {} FROM t", nest("- ", "n", "", n)))),
        ("abs()", Box::new(|n| format!("SELECT {} FROM t", nest("abs(", "n", ")", n)))),
        ("case", Box::new(|n| format!("SELECT {} FROM t", nest("CASE WHEN n > 0 THEN ", "n", " END", n)))),
        ("cast()", Box::new(|n| format!("SELECT {} FROM t", nest("CAST(", "n", " AS INT)", n)))),
        ("::", Box::new(|n| format!("SELECT n{} FROM t", "::int".repeat(n)))),
        ("or-paren", Box::new(|n| format!("SELECT {} FROM t", nest("(0 OR ", "n", ")", n)))),
        ("worst", Box::new(|n| format!("SELECT {} FROM t", nest("(0 OR 0 AND NOT 0 = '' || 0 + 0 * -(", "n", ")::int)", n)))),
        ("like", Box::new(|n| format!("SELECT {} FROM t", nest("('a' LIKE ", "s", ")", n)))),
        ("in-list", Box::new(|n| format!("SELECT {} FROM t", nest("(1 IN (", "n", "))", n)))),
        ("between", Box::new(|n| format!("SELECT {} FROM t", nest("(1 BETWEEN 0 AND ", "n", ")", n)))),
        ("scalar sub", Box::new(|n| format!("SELECT {}", nest("(SELECT ", "1", ")", n)))),
        ("scalar sub corr", Box::new(|n| format!("SELECT {} FROM t", nest("(SELECT ", "t.n", ")", n)))),
        ("scalar sub tbl", Box::new(|n| format!("SELECT {}", nest("(SELECT max(n) + ", "1", " FROM t)", n)))),
        ("from sub", Box::new(|n| format!("SELECT * FROM {}", nest("(SELECT * FROM ", "t", ") AS x", n)))),
        ("from sub fn", Box::new(|n| format!("SELECT * FROM {}", nest("(SELECT abs(n) AS n FROM ", "t", ") AS x", n)))),
        ("in sub", Box::new(|n| format!("SELECT 1 FROM t WHERE {}", nest("n IN (SELECT n FROM t WHERE ", "n > 0", ")", n)))),
        ("in sub fn", Box::new(|n| format!("SELECT 1 FROM t WHERE {}", nest("abs(n) IN (SELECT n FROM t WHERE ", "abs(n) > 0", ")", n)))),
        ("exists corr", Box::new(|n| format!("SELECT 1 FROM t a WHERE {}", nest("EXISTS (SELECT 1 FROM t b WHERE b.n = a.n AND ", "abs(a.n) > 0", ")", n)))),
        ("where not (memoire)", Box::new(|n| format!("SELECT * FROM t WHERE {}", nest("NOT ", "n = 1", "", n)))),
        ("where or-paren (memoire)", Box::new(|n| format!("SELECT * FROM t WHERE {}", nest("(n = 5 OR ", "n = 1", ")", n)))),
        ("where and-or (memoire)", Box::new(|n| format!("SELECT * FROM t WHERE {}", nest("(n = 1 AND (n = 5 OR ", "n = 1", "))", n)))),
        ("where not fn", Box::new(|n| format!("SELECT * FROM t WHERE {}", nest("NOT ", "abs(n) = 1", "", n)))),
        ("update where not", Box::new(|n| format!("UPDATE t SET n = n WHERE {}", nest("NOT ", "n = 1", "", n)))),
        ("explain", Box::new(|n| format!("{}SELECT 1", "EXPLAIN ".repeat(n)))),
        ("paren select", Box::new(|n| nest("(", "SELECT 1", ")", n))),
    ];
    for (name, make) in &shapes {
        let mut line = format!("{name:28}");
        let mut last: Option<(usize, usize, usize, usize, usize)> = None;
        for n in [8usize, 24] {
            let sql = make(n);
            let (parse_used, script) = stack_used(|| Script::parse(&sql));
            let script = match script {
                Ok(s) => s,
                Err(e) => {
                    line += &format!(" n={n}: parse error {e}");
                    continue;
                }
            };
            let (run_used, out) = stack_used(|| run(db.engine(), "main", &db.engine().schemas, &[(script.clone(), Vec::new())], true).map(|_| ()));
            let (clone_used, copy) = stack_used(|| Script { stmts: Arc::new((*script.stmts).clone()), params: 0 });
            let (dbg_used, _) = stack_used(|| format!("{:?}", script.stmts).len());
            let (eq_used, _) = stack_used(|| *script.stmts == *copy.stmts);
            drop(script);
            let (drop_used, _) = stack_used(move || drop(copy));
            if let Err(e) = out {
                line += &format!(" n={n}: run error {e}");
            }
            if let Some((n0, p0, r0, c0, d0)) = last {
                let per = |a: usize, b: usize| (a.saturating_sub(b)) / (n - n0);
                line += &format!(
                    " | per level: parse {} run {} clone {} drop {} (dbg {} eq {})",
                    per(parse_used, p0), per(run_used, r0), per(clone_used, c0), per(drop_used, d0), dbg_used / n, eq_used / n
                );
            }
            last = Some((n, parse_used, run_used, clone_used, drop_used));
        }
        if let Some((n, p, r, _, _)) = last {
            line += &format!(" | at n={n}: parse {p} run {r}");
        }
        println!("{line}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
