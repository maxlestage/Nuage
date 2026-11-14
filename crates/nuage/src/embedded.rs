//! Nuage DB inside the application: [`Embedded`], the engine without a
//! server, behind Memoire's backend traits.

use std::collections::BTreeSet;
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

/// A transaction's writes at a `SAVEPOINT`.
#[cfg_attr(not(feature = "server"), allow(dead_code))]
pub(crate) struct Savepoint {
    overlay: Overlay,
    extra: Vec<Op>,
    touched: BTreeSet<String>,
    written: BTreeSet<String>,
}

/// What a statement or a transaction read and wrote.
pub(crate) struct Work {
    snap: Snapshot,
    overlay: Overlay,
    extra: Vec<Op>,
    touched: BTreeSet<String>,
    written: BTreeSet<String>,
    /// Catalog entries already read (see [`Ctx::memo`]).
    memo: Vec<(String, std::sync::Arc<crate::engine::store::Parsed>)>,
}

impl Work {
    /// Work that may write: it goes by every commit applied so far, on disk
    /// or not yet. Whatever it gives its caller waits for them to be on disk
    /// ([`Store::wait_durable`] of [`since`](Self::since), or of its own commit).
    pub(crate) fn new(store: &Store, branch: &str) -> Result<Work> {
        Ok(Work::on(store.snapshot_latest(branch)?))
    }

    /// Work that only reads: it sees the commits that are on disk, and waits
    /// for nothing.
    pub(crate) fn reader(store: &Store, branch: &str) -> Result<Work> {
        Ok(Work::on(store.snapshot(branch)?))
    }

    fn on(snap: Snapshot) -> Work {
        Work {
            snap,
            overlay: Overlay::new(),
            extra: Vec::new(),
            touched: BTreeSet::new(),
            written: BTreeSet::new(),
            memo: Vec::new(),
        }
    }

    /// The last commit its view holds.
    pub(crate) fn since(&self) -> u64 {
        self.snap.lsn()
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
            memo: &mut self.memo,
        }
    }

    /// Commits, and returns once the commit is on disk. `check` verifies
    /// that no other commit changed the tables read since the snapshot
    /// (transactions); single statements hold the writer lock from their
    /// snapshot on, so they need no check.
    pub(crate) fn commit(self, store: &Store, branch: &str, check: bool) -> Result<()> {
        let since = self.snap.lsn();
        let read: Vec<String> = if check { self.touched.into_iter().collect() } else { Vec::new() };
        let written: Vec<String> = self.written.into_iter().collect();
        let lsn = {
            let mut w = store.writer();
            store.commit(&mut w, branch, &self.overlay, self.extra, &read, since, &written)?
        };
        // The lock is free again: the commits waiting behind this one share its sync.
        store.wait_durable(lsn)
    }
}

fn decode<O: FromRow>(rows: Vec<Vec<Value>>) -> Result<Vec<O>> {
    rows.into_iter().map(|r| O::from_row(&mut ValueRow(r), 0)).collect()
}

/// One statement or a batch, as the server and the embedded handle run them.
pub(crate) fn fetch_rows(engine: &Engine, branch: &str, query: &Query<'_>) -> Result<Vec<Vec<Value>>> {
    let mut work = Work::reader(&engine.store, branch)?;
    exec::fetch_rows(&mut work.ctx(&engine.store, branch), query)
}

pub(crate) fn execute_now(engine: &Engine, branch: &str, query: &Query<'_>) -> Result<ExecResult> {
    // The writer lock first: the snapshot then sees every earlier commit, and
    // no other commit lands before this one.
    let mut lock = engine.store.writer();
    let mut work = Work::new(&engine.store, branch)?;
    let since = work.since();
    let done = exec::execute(&mut work.ctx(&engine.store, branch), query).and_then(|result| {
        let written: Vec<String> = work.written.iter().cloned().collect();
        let lsn = engine.store.commit(&mut lock, branch, &work.overlay, work.extra, &[], since, &written)?;
        Ok((result, lsn))
    });
    drop(lock);
    // The answer, or the refusal, rests on what was read: on disk before it is given.
    match done {
        Ok((result, lsn)) => engine.store.wait_durable(lsn).map(|()| result),
        Err(e) => {
            engine.store.wait_durable(since)?;
            Err(e)
        }
    }
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
        let mut work = Work::reader(&self.engine.store, &self.branch)?;
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
        let mut work = Work::reader(&self.engine.store, &self.branch)?;
        Ok(work.ctx(&self.engine.store, &self.branch).meta(name)?.map(|m| m.schema.clone()))
    }

    /// Runs SQL (see [`crate::sql`]): one statement or several separated by
    /// `;`, in one transaction, `?` parameters from `params`. One result per
    /// statement.
    ///
    /// ```no_run
    /// # fn demo(db: &nuage::Embedded) -> memoire_orm::Result<()> {
    /// db.sql("CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)", &[])?;
    /// db.sql("INSERT INTO notes (text) VALUES (?)", &["hello".into()])?;
    /// let page = db.sql("SELECT id, upper(text) AS t FROM notes ORDER BY id DESC LIMIT 10", &[])?;
    /// println!("{:?}", page[0].rows);
    /// # Ok(()) }
    /// ```
    pub fn sql(&self, sql: &str, params: &[Json]) -> Result<Vec<crate::sql::SqlResult>> {
        let script = crate::sql::Script::parse(sql)?;
        let mut out =
            crate::sql::run(&self.engine, &self.branch, &self.engine.schemas, &[(script, params.to_vec())], true)?;
        Ok(out.pop().unwrap_or_default())
    }

    /// Statements run in one transaction: all of them apply, or none. Each
    /// is `(returns_rows, statement)`; the result is the rows, or the write's
    /// outcome. A batch that writes takes its turn from its first statement
    /// to its commit: no other commit lands in between, so it never conflicts.
    pub fn batch(
        &self,
        statements: &[(bool, Query<'_>)],
    ) -> Result<Vec<std::result::Result<Vec<Vec<Value>>, ExecResult>>> {
        let run = |tx: &EmbeddedTransaction| {
            let mut out = Vec::with_capacity(statements.len());
            for (rows, q) in statements {
                out.push(if *rows { Ok(tx.fetch_rows(q)?) } else { Err(tx.execute_now(q)?) });
            }
            Ok(out)
        };
        if statements.iter().all(|(rows, _)| *rows) {
            // Reads alone see what is on disk and wait for nothing.
            return run(&self.begin_read()?);
        }
        let store = &self.engine.store;
        let mut writer = store.writer();
        let tx = self.begin_locked()?;
        let since = tx.since()?;
        let done = run(&tx).and_then(|out| Ok((out, tx.commit_locked(&mut writer)?)));
        drop(writer);
        // The writers behind share the sync of the log this waits for.
        match done {
            Ok((out, lsn)) => store.wait_durable(lsn).map(|()| out),
            Err(e) => {
                store.wait_durable(since)?;
                Err(e)
            }
        }
    }

    /// Opens a transaction (the synchronous form of [`Database::begin`]).
    pub fn begin_now(&self) -> Result<EmbeddedTransaction> {
        Ok(EmbeddedTransaction {
            engine: self.engine.clone(),
            branch: self.branch.clone(),
            work: Mutex::new(Some(Work::new(&self.engine.store, &self.branch)?)),
            waits: true,
        })
    }

    /// A transaction that only reads: it sees the commits that are on disk
    /// and never waits for one that is being written.
    pub(crate) fn begin_read(&self) -> Result<EmbeddedTransaction> {
        Ok(EmbeddedTransaction {
            engine: self.engine.clone(),
            branch: self.branch.clone(),
            work: Mutex::new(Some(Work::reader(&self.engine.store, &self.branch)?)),
            waits: false,
        })
    }

    /// A transaction for a caller that holds the writer lock from here to
    /// [`commit_locked`](EmbeddedTransaction::commit_locked): no other commit
    /// lands meanwhile, so it cannot conflict. Its caller gives out what it
    /// read only after [`Store::wait_durable`] of its commit (or of
    /// [`since`](EmbeddedTransaction::since) when it does not commit).
    pub(crate) fn begin_locked(&self) -> Result<EmbeddedTransaction> {
        Ok(EmbeddedTransaction {
            engine: self.engine.clone(),
            branch: self.branch.clone(),
            work: Mutex::new(Some(Work::new(&self.engine.store, &self.branch)?)),
            waits: false,
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
        let mut work = Work::reader(&self.engine.store, &self.branch)?;
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
    /// What it read is given to its caller statement by statement: each
    /// answer waits for the commits it rests on to be on disk.
    waits: bool,
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
        let since = work.since();
        let done = f(&mut work.ctx(&self.engine.store, &self.branch));
        drop(guard);
        if self.waits {
            // What the transaction read may hold a commit still on its way to
            // the disk: its caller gets nothing that a crash could undo. Once
            // it is there (nearly always already), this costs one comparison.
            self.engine.store.wait_durable(since)?;
        }
        done
    }

    /// The last commit its view holds.
    pub(crate) fn since(&self) -> Result<u64> {
        let guard = self.work.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(Work::since).ok_or(Error::TransactionFinished)
    }

    /// Commits a transaction opened with [`Embedded::begin_locked`], whose
    /// caller holds the writer lock (`writer`). Returns the commit's number:
    /// the caller waits for it to be on disk once the lock is free.
    pub(crate) fn commit_locked(&self, writer: &mut crate::engine::store::Writer) -> Result<u64> {
        let work = self.work.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or(Error::TransactionFinished)?;
        let since = work.since();
        let written: Vec<String> = work.written.into_iter().collect();
        self.engine.store.commit(writer, &self.branch, &work.overlay, work.extra, &[], since, &written)
    }

    /// Rows of a statement, undecoded.
    pub fn fetch_rows(&self, query: &Query<'_>) -> Result<Vec<Vec<Value>>> {
        self.with(|ctx| exec::fetch_rows(ctx, query))
    }

    /// Runs a write in the transaction.
    pub fn execute_now(&self, query: &Query<'_>) -> Result<ExecResult> {
        self.with(|ctx| exec::execute(ctx, query))
    }

    /// Runs SQL (see [`crate::sql`]) in the transaction: one statement or
    /// several separated by `;`, `?` parameters from `params`, one result per
    /// statement. Later statements, and later calls, see what earlier ones
    /// wrote; nothing is visible outside before [`commit_now`](Self::commit_now),
    /// and [`rollback_now`](Self::rollback_now) (or dropping the transaction)
    /// undoes it all. A failing statement leaves the transaction open with
    /// the statements before it applied: roll back to discard them.
    ///
    /// ```no_run
    /// # fn demo(db: &nuage::Embedded) -> memoire_orm::Result<()> {
    /// let tx = db.begin_now()?;
    /// let seen = tx.sql("SELECT count(*) FROM notes WHERE text = ?", &["hello".into()])?;
    /// if seen[0].rows[0][0].as_i64() == Some(0) {
    ///     tx.sql("INSERT INTO notes (text) VALUES (?)", &["hello".into()])?;
    /// }
    /// tx.commit_now()?;
    /// # Ok(()) }
    /// ```
    pub fn sql(&self, sql: &str, params: &[Json]) -> Result<Vec<crate::sql::SqlResult>> {
        let script = crate::sql::Script::parse(sql)?;
        let values: Vec<Value> = params.iter().map(crate::sql::value::from_json).collect();
        crate::sql::run_in(self, &self.engine.schemas, &script, &values)
    }

    /// The table's `'static` schema as this transaction sees it.
    pub(crate) fn static_schema(
        &self,
        name: &str,
        schemas: &memoire_orm::nuage::wire::SchemaCache,
    ) -> Result<Option<&'static memoire_orm::TableSchema>> {
        self.with(|ctx| ctx.table_schema(name, schemas))
    }

    /// The tables as this transaction sees them.
    pub(crate) fn table_names(&self) -> Result<Vec<String>> {
        self.with(|ctx| ctx.table_names())
    }

    /// A change to an existing table (index, column), in the transaction.
    pub(crate) fn alter(&self, change: &exec::Alter) -> Result<()> {
        self.with(|ctx| exec::alter(ctx, change))
    }

    /// What the transaction wrote so far, to come back to (`SAVEPOINT`).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn savepoint(&self) -> Result<Savepoint> {
        let guard = self.work.lock().unwrap_or_else(|e| e.into_inner());
        let w = guard.as_ref().ok_or(Error::TransactionFinished)?;
        Ok(Savepoint {
            overlay: w.overlay.clone(),
            extra: w.extra.clone(),
            touched: w.touched.clone(),
            written: w.written.clone(),
        })
    }

    /// Forgets what the transaction wrote since `sp` (`ROLLBACK TO SAVEPOINT`).
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn rollback_to(&self, sp: &Savepoint) -> Result<()> {
        let mut guard = self.work.lock().unwrap_or_else(|e| e.into_inner());
        let w = guard.as_mut().ok_or(Error::TransactionFinished)?;
        w.overlay = sp.overlay.clone();
        w.extra = sp.extra.clone();
        w.touched = sp.touched.clone();
        w.written = sp.written.clone();
        // The catalog may be among what is forgotten.
        w.memo.clear();
        Ok(())
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
