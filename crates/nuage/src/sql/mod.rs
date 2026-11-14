//! SQL: what websites and the console send (`POST /v1/sql`,
//! [`Embedded::sql`](crate::Embedded::sql)).
//!
//! Nuage DB runs Memoire's statements, not SQL; this module reads SQL and
//! runs it on them. It covers what applications write by hand:
//!
//! * `SELECT [DISTINCT] ... FROM t [JOIN | LEFT JOIN | CROSS JOIN ... ON | USING]
//!   [WHERE] [GROUP BY] [HAVING] [UNION | INTERSECT | EXCEPT] [ORDER BY ... NULLS
//!   FIRST/LAST] [LIMIT n OFFSET m]`, subqueries (`IN`, `EXISTS`, scalar,
//!   correlated, in `FROM`), `CASE`, `CAST` and `::`, `||`, `%`, `LIKE`,
//!   `ILIKE`, `BETWEEN`, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, aggregates
//!   (`count`, `sum`, `avg`, `min`, `max`, `total`, `group_concat`,
//!   `string_agg`, `array_agg`, with `DISTINCT`) and scalar functions (text,
//!   numbers, dates, JSON: see [`value::scalar`]);
//! * `INSERT [OR REPLACE | OR IGNORE] ... VALUES | SELECT | DEFAULT VALUES
//!   [ON CONFLICT ... DO NOTHING | DO UPDATE SET ... excluded.x]
//!   [RETURNING]`, `REPLACE INTO`, MySQL's `ON DUPLICATE KEY UPDATE`;
//! * `UPDATE ... SET col = <any expression> [WHERE] [RETURNING]`,
//!   `DELETE ... [WHERE] [RETURNING]`;
//! * `CREATE TABLE [IF NOT EXISTS]` (types of PostgreSQL, MySQL and SQLite,
//!   `PRIMARY KEY`, `AUTOINCREMENT`/`SERIAL`, `NOT NULL`, `UNIQUE`, `DEFAULT`,
//!   `REFERENCES`, `UNIQUE (a, b)`), `CREATE TABLE ... AS SELECT`,
//!   `CREATE [UNIQUE] INDEX`, `DROP INDEX`, `ALTER TABLE ... ADD COLUMN`,
//!   `DROP TABLE`;
//! * several statements separated by `;`: one transaction, all or nothing
//!   (`BEGIN` and `COMMIT` are accepted and change nothing);
//! * `?`, `?N` and `$N` parameters.
//!
//! Not there: `RIGHT`/`FULL` joins, `WITH`, window functions, triggers,
//! views, changing a primary key, a primary key of several columns.

mod ast;
mod exec;
mod lex;
mod parse;
#[cfg(feature = "server")]
mod types;
pub(crate) mod value;

#[cfg(feature = "server")]
pub(crate) use types::Typing;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use memoire_orm::nuage::wire::SchemaCache;
use memoire_orm::{Error, Result, Value};
use serde_json::Value as Json;

pub use exec::SqlResult;

use crate::embedded::Embedded;
use crate::engine::Engine;

/// Statements parsed, with the number of parameters they use. A handle:
/// cloning it shares the statements.
#[derive(Clone)]
pub(crate) struct Script {
    stmts: Arc<Vec<ast::Stmt>>,
    params: usize,
}

/// Scripts already parsed, by their text. Applications send the same
/// statements again and again with other parameters: reading the SQL each
/// time cost more than running a short statement.
static PARSED: Mutex<Option<HashMap<String, Script>>> = Mutex::new(None);
/// Scripts kept; past it, the cache starts over.
const PARSED_MAX: usize = 512;
/// The longest text kept (a long one is a one-off: a dump, a migration).
const PARSED_TEXT_MAX: usize = 4096;

impl Script {
    pub(crate) fn parse(sql: &str) -> Result<Script> {
        let keep = sql.len() <= PARSED_TEXT_MAX;
        if keep
            && let Some(script) =
                PARSED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|cache| cache.get(sql))
        {
            return Ok(script.clone());
        }
        let (stmts, params) = parse::parse(sql)?;
        let script = Script { stmts: Arc::new(stmts), params };
        if keep {
            let mut cache = PARSED.lock().unwrap_or_else(|e| e.into_inner());
            let cache = cache.get_or_insert_with(HashMap::new);
            if cache.len() >= PARSED_MAX {
                cache.clear();
            }
            cache.insert(sql.to_owned(), script.clone());
        }
        Ok(script)
    }

    /// Number of statements.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        self.stmts.len()
    }

    /// Some statement changes something.
    pub(crate) fn writes(&self) -> bool {
        self.stmts.iter().any(ast::Stmt::writes)
    }

    /// Number of parameters.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn params(&self) -> usize {
        self.params
    }

    /// One script per statement; parameters keep their numbers.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn split(&self) -> Vec<Script> {
        self.stmts.iter().map(|s| Script { stmts: Arc::new(vec![s.clone()]), params: self.params }).collect()
    }

    /// `begin`, `commit` or `rollback`, when the script is that one statement.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn control(&self) -> Option<&'static str> {
        match self.stmts.as_slice() {
            [ast::Stmt::Transaction(t)] => Some(t),
            _ => None,
        }
    }

    /// It returns rows (a `SELECT`, `EXPLAIN` or `RETURNING`), when it is one statement.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn returns_rows(&self) -> bool {
        use ast::Stmt::*;
        match self.stmts.as_slice() {
            [Select(_) | Explain(_)] => true,
            [Insert { returning, .. } | Update { returning, .. } | Delete { returning, .. }] => returning.is_some(),
            _ => false,
        }
    }

    /// PostgreSQL's command tag of statement `i` (`SELECT 3`, `INSERT 0 1`...).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn tag(&self, i: usize, r: &SqlResult) -> String {
        use ast::Stmt::*;
        match self.stmts.get(i) {
            Some(Select(_)) | Some(Explain(_)) => format!("SELECT {}", r.rows.len()),
            Some(Insert { .. }) => format!("INSERT 0 {}", r.rows_affected),
            Some(Update { .. }) => format!("UPDATE {}", r.rows_affected),
            Some(Delete { .. }) => format!("DELETE {}", r.rows_affected),
            Some(CreateTable { as_select: Some(_), .. }) => format!("SELECT {}", r.rows_affected),
            Some(CreateTable { .. }) => "CREATE TABLE".into(),
            Some(CreateIndex { .. }) => "CREATE INDEX".into(),
            Some(DropIndex { .. }) => "DROP INDEX".into(),
            Some(AddColumn { .. }) => "ALTER TABLE".into(),
            Some(DropTable { .. }) => "DROP TABLE".into(),
            Some(Transaction(t)) => t.to_uppercase(),
            None => String::new(),
        }
    }
}

/// Runs `script` inside `tx`, a transaction its caller commits or rolls back
/// (the PostgreSQL protocol's `BEGIN` ... `COMMIT`, [`EmbeddedTransaction::sql`](crate::EmbeddedTransaction::sql)).
pub(crate) fn run_in(
    tx: &crate::embedded::EmbeddedTransaction,
    schemas: &SchemaCache,
    script: &Script,
    params: &[Value],
) -> Result<Vec<SqlResult>> {
    check_params(script, params.len())?;
    let runner = exec::Runner::new(tx, schemas, params);
    script.stmts.iter().map(|s| runner.run(s)).collect()
}

/// The types of a one-statement script's parameters and result columns, from
/// the columns of its tables (read in `tx`). For the PostgreSQL protocol (feature `server`).
#[cfg(feature = "server")]
pub(crate) fn typing(tx: &crate::embedded::EmbeddedTransaction, schemas: &SchemaCache, script: &Script) -> Typing {
    let runner = exec::Runner::new(tx, schemas, &[]);
    match script.stmts.as_slice() {
        [stmt] => types::infer(&runner, stmt, script.params),
        _ => Typing { params: vec![None; script.params], ..Typing::default() },
    }
}

fn check_params(script: &Script, given: usize) -> Result<()> {
    if given != script.params {
        return Err(Error::Validation(format!(
            "SQL: {given} parameter(s) given, the statement uses {}",
            script.params
        )));
    }
    Ok(())
}

/// Runs scripts (each one statement or several, separated by `;`, with its
/// parameters) on `branch` of `engine`, all in one transaction, retried on a
/// write conflict. `commit: false` rolls it back: a dry run, what would change.
pub(crate) fn run(
    engine: &Arc<Engine>,
    branch: &str,
    schemas: &SchemaCache,
    batch: &[(Script, Vec<Json>)],
    commit: bool,
) -> Result<Vec<Vec<SqlResult>>> {
    let values: Vec<Vec<Value>> = batch.iter().map(|(_, p)| p.iter().map(value::from_json).collect()).collect();
    let batch: Vec<(&Script, &[Value])> = batch.iter().zip(&values).map(|((s, _), v)| (s, v.as_slice())).collect();
    run_values(engine, branch, schemas, &batch, commit)
}

/// [`run`] with parameters as values.
pub(crate) fn run_values(
    engine: &Arc<Engine>,
    branch: &str,
    schemas: &SchemaCache,
    batch: &[(&Script, &[Value])],
    commit: bool,
) -> Result<Vec<Vec<SqlResult>>> {
    for (script, params) in batch {
        check_params(script, params.len())?;
    }
    let all_params: Vec<&[Value]> = batch.iter().map(|(_, p)| *p).collect();
    let writes = batch.iter().any(|(s, _)| s.writes());
    let db = Embedded::from_engine(engine.clone()).on_branch(branch)?;
    let mut attempt = 0;
    loop {
        let tx = db.begin_now()?;
        let mut out = Vec::with_capacity(batch.len());
        let mut failed = None;
        'scripts: for ((script, _), params) in batch.iter().zip(all_params.iter().copied()) {
            let runner = exec::Runner::new(&tx, schemas, params);
            let mut results = Vec::with_capacity(script.stmts.len());
            for s in script.stmts.iter() {
                match runner.run(s) {
                    Ok(r) => results.push(r),
                    Err(e) => {
                        failed = Some(e);
                        break 'scripts;
                    }
                }
            }
            out.push(results);
        }
        if let Some(e) = failed {
            let _ = tx.rollback_now();
            return Err(e);
        }
        if !commit || !writes {
            let _ = tx.rollback_now();
            return Ok(out);
        }
        match tx.commit_now() {
            Ok(()) => return Ok(out),
            Err(Error::Conflict(_)) if attempt < 8 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(1 << attempt.min(5)));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A value as plain JSON (what `/v1/sql` answers).
pub fn value_to_json(v: &Value) -> Json {
    value::to_json(v)
}
