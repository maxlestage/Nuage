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
pub(crate) mod value;

use std::sync::Arc;

use memoire_orm::nuage::wire::SchemaCache;
use memoire_orm::{Error, Result, Value};
use serde_json::Value as Json;

pub use exec::SqlResult;

use crate::embedded::Embedded;
use crate::engine::Engine;

/// Statements parsed, with the number of parameters they use.
pub(crate) struct Script {
    stmts: Vec<ast::Stmt>,
    params: usize,
}

impl Script {
    pub(crate) fn parse(sql: &str) -> Result<Script> {
        let (stmts, params) = parse::parse(sql)?;
        Ok(Script { stmts, params })
    }

    /// Number of statements.
    pub(crate) fn len(&self) -> usize {
        self.stmts.len()
    }

    /// Some statement changes something.
    pub(crate) fn writes(&self) -> bool {
        self.stmts.iter().any(ast::Stmt::writes)
    }
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
    let mut all_params = Vec::with_capacity(batch.len());
    for (script, params) in batch {
        if params.len() != script.params {
            return Err(Error::Validation(format!(
                "SQL: {} parameter(s) given, the statement uses {}",
                params.len(),
                script.params
            )));
        }
        all_params.push(params.iter().map(value::from_json).collect::<Vec<Value>>());
    }
    let writes = batch.iter().any(|(s, _)| s.writes());
    let db = Embedded::from_engine(engine.clone()).on_branch(branch)?;
    let mut attempt = 0;
    loop {
        let tx = db.begin_now()?;
        let mut out = Vec::with_capacity(batch.len());
        let mut failed = None;
        'scripts: for ((script, _), params) in batch.iter().zip(&all_params) {
            let runner = exec::Runner::new(&tx, schemas, params);
            let mut results = Vec::with_capacity(script.stmts.len());
            for s in &script.stmts {
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
