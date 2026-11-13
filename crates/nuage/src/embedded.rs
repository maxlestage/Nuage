//! Nuage DB inside the application: [`Embedded`], the engine without a
//! server, behind Memoire's backend traits.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use memoire_orm::evaluator;
use memoire_orm::{
    Database, Dialect, Error, ExecResult, Executor, FromRow, Query, Result, Transaction, Value, ValueRow,
};
use serde_json::Value as Json;

use crate::engine::exec::{self, Ctx, Fetched};
use crate::engine::store::{Overlay, Snapshot, Store};
use crate::engine::wal::Op;
use crate::engine::{Engine, EngineOptions, MAIN};

/// What a statement or a transaction read and wrote.
pub(crate) struct Work {
    snap: Snapshot,
    overlay: Overlay,
    extra: Vec<Op>,
    touched: BTreeMap<String, u64>,
    written: BTreeSet<String>,
}

impl Work {
    pub(crate) fn new(store: &Store, branch: &str) -> Result<Work> {
        Ok(Work {
            snap: store.snapshot(branch)?,
            overlay: Overlay::new(),
            extra: Vec::new(),
            touched: BTreeMap::new(),
            written: BTreeSet::new(),
        })
    }

    pub(crate) fn ctx<'a>(&'a mut self, store: &'a Store, branch: &'a str) -> Ctx<'a> {
        Ctx {
            store,
            snap: &self.snap,
            branch,
            overlay: &mut self.overlay,
            extra: &mut self.extra,
            touched: &mut self.touched,
            written: &mut self.written,
        }
    }

    /// Commits. `check` verifies that no other commit changed the tables read
    /// since the snapshot (transactions); single statements hold the writer
    /// lock from their snapshot on, so they need no check.
    pub(crate) fn commit(self, store: &Store, branch: &str, check: bool) -> Result<()> {
        let mut w = store.writer();
        let expect: Vec<(String, u64)> = if check { self.touched.into_iter().collect() } else { Vec::new() };
        let written: Vec<String> = self.written.into_iter().collect();
        store.commit(&mut w, branch, &self.overlay, self.extra, &expect, &written)?;
        Ok(())
    }
}

fn decode<O: FromRow>(rows: Vec<Vec<Value>>) -> Result<Vec<O>> {
    rows.into_iter().map(|r| O::from_row(&mut ValueRow(r), 0)).collect()
}

/// One statement or a batch, as the server and the embedded handle run them.
pub(crate) fn fetch_rows(engine: &Engine, branch: &str, query: &Query<'_>) -> Result<Vec<Vec<Value>>> {
    let mut work = Work::new(&engine.store, branch)?;
    exec::fetch_rows(&mut work.ctx(&engine.store, branch), query)
}

pub(crate) fn execute_now(engine: &Engine, branch: &str, query: &Query<'_>) -> Result<ExecResult> {
    // The writer lock first: the snapshot then sees every earlier commit, and
    // no other commit lands before this one.
    let mut lock = engine.store.writer();
    let mut work = Work::new(&engine.store, branch)?;
    let result = exec::execute(&mut work.ctx(&engine.store, branch), query)?;
    let written: Vec<String> = work.written.iter().cloned().collect();
    engine.store.commit(&mut lock, branch, &work.overlay, work.extra, &[], &written)?;
    Ok(result)
}

/// A table, as the console lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct TableInfo {
    /// Table name.
    pub name: String,
    /// Number of rows.
    pub rows: i64,
    /// Bytes of its stored rows (indexes not counted).
    pub bytes: i64,
    /// Stored column names.
    pub columns: Vec<String>,
    /// The schema of the model that created (or last widened) it, as JSON
    /// (see `memoire_orm::nuage::wire::schema_to_json`).
    pub schema: Json,
}

/// Nuage DB embedded in the application: the same engine as the server, in
/// the same process, behind Memoire's [`Executor`] and [`Database`] traits.
///
/// ```no_run
/// # async fn demo() -> memoire_orm::Result<()> {
/// use memoire_orm::prelude::*;
/// use nuage::Embedded;
///
/// #[derive(Model, Debug)]
/// struct Note { #[memoire(primary_key, auto_increment)] id: i64, text: String }
///
/// let db = Embedded::open("data")?;
/// db.create_table_if_not_exists::<Note>().await?;
/// Note { id: 0, text: "hello".into() }.insert(&db).await?;
/// # Ok(()) }
/// ```
///
/// Cloning is cheap; clones share the engine. Opening the same directory
/// twice in one process is refused by the page store: share one handle.
#[derive(Clone)]
pub struct Embedded {
    engine: Arc<Engine>,
    branch: String,
}

impl std::fmt::Debug for Embedded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedded").field("dir", &self.engine.dir()).field("branch", &self.branch).finish()
    }
}

impl Embedded {
    /// Opens (or creates) a database in `dir`, with the default options.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(dir, EngineOptions::default())
    }

    /// Opens (or creates) a database in `dir`.
    pub fn open_with(dir: impl AsRef<Path>, options: EngineOptions) -> Result<Self> {
        Ok(Self::from_engine(Engine::open(dir, options)?))
    }

    /// A handle on an engine already open (shared with a server...).
    pub fn from_engine(engine: Arc<Engine>) -> Self {
        Embedded { engine, branch: MAIN.to_owned() }
    }

    /// The engine.
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// The same database seen through a branch.
    pub fn on_branch(&self, name: &str) -> Result<Embedded> {
        if !self.engine.has_branch(name) {
            return Err(Error::NotFound);
        }
        Ok(Embedded { engine: self.engine.clone(), branch: name.to_owned() })
    }

    /// The branch this handle runs on (`main` for the main database).
    pub fn branch_name(&self) -> &str {
        &self.branch
    }

    /// Rows of a statement, undecoded.
    pub fn fetch_rows(&self, query: Query<'_>) -> Result<Vec<Vec<Value>>> {
        fetch_rows(&self.engine, &self.branch, &query)
    }

    /// The tables of the branch, with their row counts.
    pub fn tables(&self) -> Result<Vec<TableInfo>> {
        let mut work = Work::new(&self.engine.store, &self.branch)?;
        let mut ctx = work.ctx(&self.engine.store, &self.branch);
        let mut out = Vec::new();
        for name in ctx.table_names()? {
            if let Some(meta) = ctx.meta(&name)? {
                out.push(TableInfo {
                    rows: ctx.row_count(&meta)?,
                    bytes: ctx.table_bytes(&meta)?,
                    columns: meta.columns.clone(),
                    schema: meta.schema.clone(),
                    name,
                });
            }
        }
        Ok(out)
    }

    /// The schema of one table (as `memoire_orm::nuage::wire` writes it),
    /// `None` when there is no such table: one read, whatever the number of
    /// tables.
    pub fn table_schema(&self, name: &str) -> Result<Option<Json>> {
        let mut work = Work::new(&self.engine.store, &self.branch)?;
        Ok(work.ctx(&self.engine.store, &self.branch).meta(name)?.map(|m| m.schema))
    }

    /// Statements run in one transaction: all of them apply, or none. Each
    /// is `(returns_rows, statement)`; the result is the rows, or the write's
    /// outcome.
    pub fn batch(
        &self,
        statements: &[(bool, Query<'_>)],
    ) -> Result<Vec<std::result::Result<Vec<Vec<Value>>, ExecResult>>> {
        let tx = self.begin_now()?;
        let mut out = Vec::with_capacity(statements.len());
        for (rows, q) in statements {
            out.push(if *rows { Ok(tx.fetch_rows(q)?) } else { Err(tx.execute_now(q)?) });
        }
        tx.commit_now()?;
        Ok(out)
    }

    /// Opens a transaction (the synchronous form of [`Database::begin`]).
    pub fn begin_now(&self) -> Result<EmbeddedTransaction> {
        Ok(EmbeddedTransaction {
            engine: self.engine.clone(),
            branch: self.branch.clone(),
            work: Mutex::new(Some(Work::new(&self.engine.store, &self.branch)?)),
        })
    }
}

impl Executor for Embedded {
    /// `None`: Memoire statements are evaluated directly, without SQL.
    fn dialect(&self) -> Option<Dialect> {
        None
    }

    async fn fetch_all<O>(&self, query: Query<'_>) -> Result<Vec<O>>
    where
        O: FromRow + Send,
    {
        let mut work = Work::new(&self.engine.store, &self.branch)?;
        match exec::fetch(&mut work.ctx(&self.engine.store, &self.branch), &query)? {
            Fetched::Rows(rows) => decode(rows),
            Fetched::Tables(t) => evaluator::fetch_as(&t, query),
        }
    }

    async fn execute(&self, query: Query<'_>) -> Result<ExecResult> {
        execute_now(&self.engine, &self.branch, &query)
    }
}

impl Database for Embedded {
    type Tx<'c> = EmbeddedTransaction;

    async fn begin(&self) -> Result<EmbeddedTransaction> {
        self.begin_now()
    }
}

/// A transaction of [`Embedded`]: it reads a snapshot taken when it began,
/// plus its own changes, and commits them at once. A commit fails with
/// [`Error::Conflict`] when another commit changed a table it used since.
pub struct EmbeddedTransaction {
    engine: Arc<Engine>,
    branch: String,
    work: Mutex<Option<Work>>,
}

impl std::fmt::Debug for EmbeddedTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedTransaction").field("branch", &self.branch).finish_non_exhaustive()
    }
}

impl EmbeddedTransaction {
    fn with<R>(&self, f: impl FnOnce(&mut Ctx<'_>) -> Result<R>) -> Result<R> {
        let mut guard = self.work.lock().unwrap_or_else(|e| e.into_inner());
        let work = guard.as_mut().ok_or(Error::TransactionFinished)?;
        f(&mut work.ctx(&self.engine.store, &self.branch))
    }

    /// Rows of a statement, undecoded.
    pub fn fetch_rows(&self, query: &Query<'_>) -> Result<Vec<Vec<Value>>> {
        self.with(|ctx| exec::fetch_rows(ctx, query))
    }

    /// Runs a write in the transaction.
    pub fn execute_now(&self, query: &Query<'_>) -> Result<ExecResult> {
        self.with(|ctx| exec::execute(ctx, query))
    }

    /// Commits (the synchronous form of [`Transaction::commit`]).
    pub fn commit_now(&self) -> Result<()> {
        let work = self.work.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or(Error::TransactionFinished)?;
        work.commit(&self.engine.store, &self.branch, true)
    }

    /// Rolls back (the synchronous form of [`Transaction::rollback`]).
    pub fn rollback_now(&self) -> Result<()> {
        self.work.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or(Error::TransactionFinished).map(|_| ())
    }
}

impl Executor for EmbeddedTransaction {
    fn dialect(&self) -> Option<Dialect> {
        None
    }

    async fn fetch_all<O>(&self, query: Query<'_>) -> Result<Vec<O>>
    where
        O: FromRow + Send,
    {
        let fetched = self.with(|ctx| exec::fetch(ctx, &query))?;
        match fetched {
            Fetched::Rows(rows) => decode(rows),
            Fetched::Tables(t) => evaluator::fetch_as(&t, query),
        }
    }

    async fn execute(&self, query: Query<'_>) -> Result<ExecResult> {
        self.execute_now(&query)
    }
}

impl Transaction for EmbeddedTransaction {
    async fn commit(self) -> Result<()> {
        self.commit_now()
    }

    async fn rollback(self) -> Result<()> {
        self.rollback_now()
    }
}
