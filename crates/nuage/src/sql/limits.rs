//! What a statement may use: memory and time.
//!
//! A statement holds rows in memory while it works (what it read, joined,
//! grouped, sorted, and its answer): nothing else of the server grows with
//! what one request asks for. Those rows are counted, by an estimate of their
//! size, against a budget; past it the statement fails instead of taking the
//! memory of the whole server. The statements of one request share the budget:
//! their answers are all held until the request is answered.
//!
//! The requests running at the same time also share one budget, a quarter of
//! the memory of the server: each one fitting its own budget, many of them
//! together would still take it all.
//!
//! A statement also has a deadline, read in the loops that run once per row
//! and as the functions it calls get through their texts: past it, it fails
//! instead of keeping its thread.
//!
//! Both come from two settings of the process ([`set_statement_memory`],
//! [`set_statement_timeout`]), read when a request starts.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use memoire_orm::{Error, Result, Value};
use serde_json::Value as Json;

/// The largest text (or JSON array) a function or an operator may build:
/// `repeat`, `lpad`, `||`, `string_agg`... are asked for sizes, and the size
/// is checked before the memory is taken.
pub(crate) const MAX_TEXT: usize = 64 << 20;

/// Values an expression keeps while it computes the next ones are counted
/// once they add up to this (see [`Live`]): smaller ones cost nothing to
/// count, and a statement nests too little for them to add up.
const LIVE_STEP: usize = 64 << 10;

/// Bytes of texts the functions of a statement get through between two
/// readings of the clock (see [`Limits::work`]).
const WORK_STEP: usize = 1 << 20;

/// A request takes its share of what all of them may hold in steps of this:
/// one shared counter per step instead of one per row.
const RESERVE_STEP: usize = 256 << 10;

/// The budget when the memory of the server is unknown.
const MEMORY_UNKNOWN: usize = 256 << 20;
/// The budget is never under this, however small the server.
const MEMORY_MIN: usize = 32 << 20;
/// Nor over this, however large.
const MEMORY_MAX: usize = 2 << 30;
/// The smallest budget [`set_statement_memory`] takes: under it, ordinary
/// statements would fail.
const MEMORY_SETTING_MIN: usize = 1 << 20;
/// The deadline when nothing was set.
const TIMEOUT_DEFAULT_MS: u64 = 60_000;

/// The budget set, in bytes (0: the default).
static MEMORY: AtomicUsize = AtomicUsize::new(0);
/// The timeout set, in milliseconds (0: none).
static TIMEOUT_MS: AtomicU64 = AtomicU64::new(TIMEOUT_DEFAULT_MS);

/// Sets the memory the statements of one request may hold at once, in bytes
/// (the setting `NUAGE_STATEMENT_MB` of a server), for every request that
/// starts from now on, in this process: the server's and
/// [`Embedded::sql`](crate::Embedded::sql)'s alike. `0` gives the default
/// back: an eighth of the memory of the server, between 32 MiB and 2 GiB
/// (256 MiB when the memory is unknown). The smallest budget is 1 MiB.
///
/// The requests at work at the same time hold together at most a quarter of
/// the memory of the server, or this budget when it is larger: past it, a
/// statement fails and asks to be tried again. The same applies to
/// Memoire's own statements (the wire protocol, [`Embedded`](crate::Embedded)'s
/// queries): a read or a join that would not fit fails.
pub fn set_statement_memory(bytes: usize) {
    MEMORY.store(if bytes == 0 { 0 } else { bytes.max(MEMORY_SETTING_MIN) }, Ordering::Relaxed);
}

/// The memory the statements of one request may hold at once, in bytes.
pub fn statement_memory() -> usize {
    match MEMORY.load(Ordering::Relaxed) {
        0 => default_memory(),
        set => set,
    }
}

/// Sets how long one statement may run (the setting `NUAGE_STATEMENT_SECONDS`
/// of a server), for every request that starts from now on, in this process.
/// `None` lets statements run without limit. Without a call, 60 seconds.
pub fn set_statement_timeout(timeout: Option<Duration>) {
    let ms = match timeout {
        // Under a millisecond is still a deadline, not "none".
        Some(t) => u64::try_from(t.as_millis()).unwrap_or(u64::MAX).max(1),
        None => 0,
    };
    TIMEOUT_MS.store(ms, Ordering::Relaxed);
}

/// How long one statement may run (`None`: without limit).
pub fn statement_timeout() -> Option<Duration> {
    match TIMEOUT_MS.load(Ordering::Relaxed) {
        0 => None,
        ms => Some(Duration::from_millis(ms)),
    }
}

/// The memory of the server: read once, and again after
/// [`memory_changed`] (a server's owner said another memory).
fn total_memory() -> Option<u64> {
    match TOTAL.load(Ordering::Relaxed) {
        TOTAL_UNREAD => {
            let total = crate::memory::MemoryInfo::now().total_bytes;
            TOTAL.store(total.unwrap_or(TOTAL_UNKNOWN), Ordering::Relaxed);
            total
        }
        TOTAL_UNKNOWN => None,
        total => Some(total),
    }
}

/// The memory of the server as last read ([`TOTAL_UNREAD`]: not yet).
static TOTAL: AtomicU64 = AtomicU64::new(TOTAL_UNREAD);
const TOTAL_UNREAD: u64 = u64::MAX;
const TOTAL_UNKNOWN: u64 = u64::MAX - 1;

/// The memory of the server changed: the default budgets follow it.
#[cfg(feature = "server")]
pub(crate) fn memory_changed() {
    TOTAL.store(TOTAL_UNREAD, Ordering::Relaxed);
}

/// An eighth of the memory of the server: a few statements at the same
/// time, the page caches and the rest all fit.
fn default_memory() -> usize {
    match total_memory() {
        Some(total) => usize::try_from(total / 8).unwrap_or(MEMORY_MAX).clamp(MEMORY_MIN, MEMORY_MAX),
        None => MEMORY_UNKNOWN,
    }
}

/// What the statements of all requests may hold together: a quarter of the
/// memory of the server (two budgets when it is unknown), and never less
/// than the budget of one request. Rows cost more than they are counted
/// (the allocator's share, their answer once written as JSON): a quarter
/// counted leaves the rest of the memory to that and to the caches.
fn shared_memory() -> usize {
    let share = match total_memory() {
        Some(total) => usize::try_from(total / 4).unwrap_or(usize::MAX).max(MEMORY_MIN),
        None => 2 * MEMORY_UNKNOWN,
    };
    share.max(statement_memory())
}

/// What the requests at work hold together, by steps of [`RESERVE_STEP`].
#[derive(Debug)]
pub(crate) struct Pool {
    held: AtomicUsize,
    /// What they may hold (0: [`shared_memory`]).
    cap: usize,
    /// The request that may go on past `cap` (0: none yet). Requests that
    /// grow together would otherwise all be refused before any ends: one of
    /// them goes on, ends, and gives the room back.
    first: AtomicU64,
}

/// The requests of the process.
static POOL: Pool = Pool { held: AtomicUsize::new(0), cap: 0, first: AtomicU64::new(0) };

/// The number of the next request.
static REQUESTS: AtomicU64 = AtomicU64::new(1);

impl Pool {
    fn cap(&self) -> usize {
        if self.cap == 0 { shared_memory() } else { self.cap }
    }

    /// Takes `bytes` more, when they fit.
    fn take(&self, bytes: usize) -> bool {
        let cap = self.cap();
        let mut held = self.held.load(Ordering::Acquire);
        loop {
            let Some(more) = held.checked_add(bytes).filter(|n| *n <= cap) else { return false };
            match self.held.compare_exchange_weak(held, more, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => held = now,
            }
        }
    }

    fn give(&self, bytes: usize) {
        self.held.fetch_sub(bytes, Ordering::AcqRel);
    }

    fn room(&self) -> usize {
        self.cap().saturating_sub(self.held.load(Ordering::Acquire))
    }

    /// Request `id` is the one that goes on past the cap, when none is.
    fn go_on(&self, id: u64) -> bool {
        match self.first.compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => true,
            Err(first) => first == id,
        }
    }
}

/// The budget and the deadline of the statements of one request.
#[derive(Debug)]
pub(crate) struct Limits {
    /// Bytes the statements may hold at once.
    memory: usize,
    /// Bytes held now.
    held: Cell<usize>,
    /// Bytes taken from what all requests may hold: `held` and a little more.
    reserved: Cell<usize>,
    pool: &'static Pool,
    /// How long each statement may run.
    timeout: Option<Duration>,
    /// When the statement at work must have ended.
    deadline: Cell<Option<Instant>>,
    /// Steps since the clock was last read.
    steps: Cell<u32>,
    /// Bytes of texts gone through since the clock was last read.
    work: Cell<usize>,
    /// The engine left a join to the SQL layer (see [`Limits::decline`]).
    declined: Cell<bool>,
    /// The beginning of its errors: `SQL: ` for SQL statements.
    who: &'static str,
    /// Its number among the requests of the process.
    id: u64,
}

/// Steps between two readings of the clock: reading it at each row would cost
/// more than many rows do.
const STEPS: u32 = 1024;

thread_local! {
    /// The limits of the SQL statement this thread is running, for the scans
    /// of the engine it calls (see [`at_work`]).
    static AT_WORK: RefCell<Option<Rc<Limits>>> = const { RefCell::new(None) };
}

/// The limits of the SQL statement this thread is running: the engine's scans
/// read them to stop when it is out of memory or time. `None` outside SQL
/// (Memoire's own statements).
pub(crate) fn at_work() -> Option<Rc<Limits>> {
    AT_WORK.with(|l| l.borrow().clone())
}

/// A statement at work, until dropped.
pub(crate) struct AtWork {
    before: Option<Rc<Limits>>,
}

impl Drop for AtWork {
    fn drop(&mut self) {
        let before = self.before.take();
        AT_WORK.with(|l| *l.borrow_mut() = before);
    }
}

impl Limits {
    /// The limits of a SQL request that starts now, from the settings.
    pub(crate) fn from_settings() -> Rc<Limits> {
        Limits::new(statement_memory(), statement_timeout())
    }

    /// The limits of one of Memoire's statements that starts now (the wire
    /// protocol, [`Embedded`](crate::Embedded)'s own queries), from the settings.
    pub(crate) fn for_memoire() -> Rc<Limits> {
        Limits::make(statement_memory(), statement_timeout(), &POOL, "")
    }

    pub(crate) fn new(memory: usize, timeout: Option<Duration>) -> Rc<Limits> {
        Limits::make(memory, timeout, &POOL, "SQL: ")
    }

    fn make(memory: usize, timeout: Option<Duration>, pool: &'static Pool, who: &'static str) -> Rc<Limits> {
        Rc::new(Limits {
            memory,
            held: Cell::new(0),
            reserved: Cell::new(0),
            pool,
            timeout,
            deadline: Cell::new(None),
            steps: Cell::new(0),
            work: Cell::new(0),
            declined: Cell::new(false),
            who,
            id: REQUESTS.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// A statement starts: its deadline runs from now, and the engine's scans
    /// see these limits until the guard is dropped.
    pub(crate) fn begin(self: &Rc<Self>) -> AtWork {
        self.deadline.set(self.timeout.and_then(|t| Instant::now().checked_add(t)));
        self.declined.set(false);
        AtWork { before: AT_WORK.with(|l| l.borrow_mut().replace(self.clone())) }
    }

    fn out_of_memory(&self) -> Error {
        Error::Validation(format!(
            "{}the statement needs more than {} MiB of memory: add a LIMIT or a filter, or raise NUAGE_STATEMENT_MB",
            self.who,
            (self.memory >> 20).max(1)
        ))
    }

    /// What the requests at work hold together is all they may hold.
    fn busy(&self) -> Error {
        Error::Validation(format!(
            "{}the statements running at this moment hold all the memory they may share ({} MiB): try again in a \
             moment, or add a LIMIT or a filter",
            self.who,
            (self.pool.cap() >> 20).max(1)
        ))
    }

    /// The engine will not join the tables of one of Memoire's statements:
    /// the pairs would not fit.
    pub(crate) fn join_too_large(&self) -> Error {
        Error::Validation(format!(
            "{}the join may give more rows than a statement may hold ({} MiB): add a filter, or raise \
             NUAGE_STATEMENT_MB",
            self.who,
            (self.memory >> 20).max(1)
        ))
    }

    /// Counts `bytes` more as held.
    #[inline]
    pub(crate) fn charge(&self, bytes: usize) -> Result<()> {
        let held = self.held.get().saturating_add(bytes);
        if held > self.memory {
            return Err(self.out_of_memory());
        }
        if held > self.reserved.get() {
            self.reserve(held)?;
        }
        self.held.set(held);
        Ok(())
    }

    /// Takes from what all requests may hold enough for `held`, a step more.
    #[cold]
    fn reserve(&self, held: usize) -> Result<()> {
        let want = held.div_ceil(RESERVE_STEP).saturating_add(1).saturating_mul(RESERVE_STEP);
        let more = want - self.reserved.get();
        if !self.pool.take(more) {
            if !self.pool.go_on(self.id) {
                return Err(self.busy());
            }
            // Past the cap: by what this request needs at most, one request at a time.
            self.pool.held.fetch_add(more, Ordering::AcqRel);
        }
        self.reserved.set(want);
        Ok(())
    }

    /// What the request no longer holds goes back to the others, once it is
    /// more than a step or two: rows come and go a little at a time.
    #[inline]
    fn give_back(&self) {
        let keep = self.held.get().div_ceil(RESERVE_STEP).saturating_add(1).saturating_mul(RESERVE_STEP);
        let reserved = self.reserved.get();
        if reserved > keep.saturating_add(RESERVE_STEP) {
            self.pool.give(reserved - keep);
            self.reserved.set(keep);
        }
    }

    /// `bytes` counted before are no longer held.
    #[inline]
    pub(crate) fn release(&self, bytes: usize) {
        self.held.set(self.held.get().saturating_sub(bytes));
        self.give_back();
    }

    /// Fails when `bytes` more would not fit, without counting them: for the
    /// engine's scans, whose rows are counted by the SQL layer once it has them.
    pub(crate) fn fits(&self, bytes: usize) -> Result<()> {
        let held = self.held.get().saturating_add(bytes);
        if held > self.memory {
            return Err(self.out_of_memory());
        }
        if held > self.reserved.get() && held - self.reserved.get() > self.pool.room() && !self.pool.go_on(self.id) {
            return Err(self.busy());
        }
        Ok(())
    }

    /// Bytes held now.
    pub(crate) fn held(&self) -> usize {
        self.held.get()
    }

    /// A statement ended: `bytes` are held (the answers so far).
    pub(crate) fn settle(&self, bytes: usize) {
        self.held.set(bytes);
        self.give_back();
    }

    /// One step of a loop over rows: every so many, the clock is read.
    #[inline]
    pub(crate) fn tick(&self) -> Result<()> {
        let n = self.steps.get().wrapping_add(1);
        self.steps.set(n);
        if n.is_multiple_of(STEPS) { self.check_time() } else { Ok(()) }
    }

    /// A function went through `bytes` of text: every few of them, the clock
    /// is read. One row may ask for hours of work (a long text, many times):
    /// the deadline bounds the work, not only the rows.
    #[inline]
    pub(crate) fn work(&self, bytes: usize) -> Result<()> {
        let done = self.work.get().saturating_add(bytes);
        if done < WORK_STEP {
            self.work.set(done);
            return Ok(());
        }
        self.work.set(0);
        self.check_time()
    }

    /// Fails when the statement ran past its deadline.
    pub(crate) fn check_time(&self) -> Result<()> {
        match self.deadline.get() {
            Some(deadline) if Instant::now() >= deadline => {
                let limit = self.timeout.unwrap_or_default();
                Err(Error::Validation(format!(
                    "{}statement timeout: it ran for more than {} (NUAGE_STATEMENT_SECONDS)",
                    self.who,
                    if limit.as_secs() >= 1 {
                        format!("{} second(s)", limit.as_secs())
                    } else {
                        format!("{} ms", limit.as_millis())
                    }
                )))
            }
            _ => Ok(()),
        }
    }

    /// Values kept while the next ones are computed, counted until dropped.
    #[inline]
    pub(crate) fn live(&self) -> Live<'_> {
        Live { limits: self, charged: 0, pending: 0 }
    }

    /// The engine will not join these tables itself (it would hold the pairs
    /// outside the budget): it says so, and the SQL layer joins them, row by
    /// row counted.
    pub(crate) fn decline(&self) {
        self.declined.set(true);
    }

    /// The engine declined since the last call.
    pub(crate) fn take_declined(&self) -> bool {
        self.declined.replace(false)
    }

    /// Nothing held yet, to add to.
    pub(crate) fn hold(self: &Rc<Self>) -> Hold {
        Hold { limits: self.clone(), bytes: 0 }
    }

    /// `rows` counted as held.
    pub(crate) fn hold_rows(self: &Rc<Self>, rows: &[Vec<Value>]) -> Result<Hold> {
        let mut hold = self.hold();
        hold.add(rows_bytes(rows))?;
        Ok(hold)
    }
}

/// Bytes counted as held, until dropped: the rows of a step of a statement.
/// It goes where the rows go, and is dropped with them.
#[derive(Debug)]
pub(crate) struct Hold {
    limits: Rc<Limits>,
    bytes: usize,
}

impl Hold {
    /// `bytes` more.
    #[inline]
    pub(crate) fn add(&mut self, bytes: usize) -> Result<()> {
        self.limits.charge(bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    /// What it holds is now `bytes` (rows were dropped, or added).
    pub(crate) fn set(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.bytes {
            return self.add(bytes - self.bytes);
        }
        self.limits.release(self.bytes - bytes);
        self.bytes = bytes;
        Ok(())
    }

    /// `bytes` of what it held were dropped.
    pub(crate) fn less(&mut self, bytes: usize) {
        let bytes = bytes.min(self.bytes);
        self.limits.release(bytes);
        self.bytes -= bytes;
    }

    /// Takes what `other` held: its rows were moved into these.
    pub(crate) fn absorb(&mut self, mut other: Hold) {
        self.bytes += std::mem::take(&mut other.bytes);
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.limits.release(self.bytes);
    }
}

impl Drop for Limits {
    fn drop(&mut self) {
        self.pool.give(self.reserved.get());
        let _ = self.pool.first.compare_exchange(self.id, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// Values an expression keeps while it computes the next ones: the arguments
/// of a function, the left side of a comparison, the items of `IN (...)`.
/// Each may be a text of many MiB, and an expression has many of them: they
/// are counted as held once they add up to [`LIVE_STEP`], until dropped.
pub(crate) struct Live<'l> {
    limits: &'l Limits,
    /// Bytes counted as held.
    charged: usize,
    /// Bytes kept, not counted yet.
    pending: usize,
}

impl Live<'_> {
    /// `v` is kept too.
    #[inline]
    pub(crate) fn add(&mut self, v: &Value) -> Result<()> {
        self.add_bytes(value_bytes(v))
    }

    /// `bytes` more are kept.
    #[inline]
    pub(crate) fn add_bytes(&mut self, bytes: usize) -> Result<()> {
        self.pending = self.pending.saturating_add(bytes);
        if self.pending >= LIVE_STEP {
            self.limits.charge(self.pending)?;
            self.charged += self.pending;
            self.pending = 0;
        }
        Ok(())
    }

    /// All the bytes kept so far.
    #[inline]
    pub(crate) fn bytes(&self) -> usize {
        self.charged + self.pending
    }
}

impl Drop for Live<'_> {
    #[inline]
    fn drop(&mut self) {
        if self.charged > 0 {
            self.limits.release(self.charged);
        }
    }
}

fn json_bytes(j: &Json) -> usize {
    match j {
        Json::String(s) => s.len(),
        Json::Array(items) => items.iter().map(|x| size_of::<Json>() + json_bytes(x)).sum(),
        Json::Object(map) => map.iter().map(|(k, x)| 2 * size_of::<Json>() + k.len() + json_bytes(x)).sum(),
        _ => 0,
    }
}

/// What the allocator takes beside each block it gives.
const BLOCK: usize = 16;

/// About what a value takes in memory.
#[inline]
pub(crate) fn value_bytes(v: &Value) -> usize {
    size_of::<Value>()
        + match v {
            Value::Text(s) => s.len() + BLOCK,
            Value::Bytes(b) => b.len() + BLOCK,
            Value::Json(j) => json_bytes(j) + BLOCK,
            _ => 0,
        }
}

/// About what a row takes in memory.
#[inline]
pub(crate) fn row_bytes(row: &[Value]) -> usize {
    size_of::<Vec<Value>>() + BLOCK + row.iter().map(value_bytes).sum::<usize>()
}

/// About what rows take in memory.
pub(crate) fn rows_bytes(rows: &[Vec<Value>]) -> usize {
    rows.iter().map(|r| row_bytes(r)).sum()
}

/// The error of a function or an operator asked for a text larger than
/// [`MAX_TEXT`]: `what` names it (`repeat()`, `||`). A large one must also
/// fit in what the statement at work may still hold: it is checked before
/// it is built, not once it is there.
pub(crate) fn text_fits(what: &str, bytes: usize) -> Result<()> {
    if bytes > MAX_TEXT {
        return Err(Error::Conversion(format!("{what}: the result would be larger than {} MiB", MAX_TEXT >> 20)));
    }
    if bytes >= LIVE_STEP {
        AT_WORK.with(|l| l.borrow().as_ref().map_or(Ok(()), |limits| limits.fits(bytes)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_counts_what_is_held() {
        let limits = Limits::new(1000, None);
        let mut a = limits.hold();
        a.add(600).unwrap();
        assert!(limits.fits(400).is_ok() && limits.fits(401).is_err());
        let mut b = limits.hold();
        let refused = b.add(500).unwrap_err().to_string();
        assert!(refused.contains("NUAGE_STATEMENT_MB") && refused.contains("LIMIT"), "{refused}");
        // A refusal counts nothing.
        assert_eq!(limits.held(), 600);
        b.add(300).unwrap();
        a.set(100).unwrap();
        assert_eq!(limits.held(), 400);
        a.absorb(b);
        assert_eq!(limits.held(), 400);
        drop(a);
        assert_eq!(limits.held(), 0);
    }

    #[test]
    fn a_deadline_is_per_statement() {
        let limits = Limits::new(1000, Some(Duration::from_millis(30)));
        // No statement at work: no deadline.
        assert!(limits.check_time().is_ok());
        let first = limits.begin();
        assert!(at_work().is_some());
        std::thread::sleep(Duration::from_millis(40));
        let late = limits.check_time().unwrap_err().to_string();
        assert!(late.contains("statement timeout") && late.contains("NUAGE_STATEMENT_SECONDS"), "{late}");
        // The clock is read every so many steps.
        assert!((0..STEPS * 2).any(|_| limits.tick().is_err()));
        drop(first);
        assert!(at_work().is_none());
        // The next statement has its own.
        let _second = limits.begin();
        assert!(limits.check_time().is_ok());
    }

    #[test]
    fn sizes_follow_the_values() {
        let text = Value::Text("x".repeat(1000));
        assert!(value_bytes(&text) >= 1000 && value_bytes(&Value::I64(1)) < 100);
        let doc = Value::Json(serde_json::json!({ "a": ["x".repeat(500), 1, null], "b": { "c": "y".repeat(500) } }));
        assert!(value_bytes(&doc) >= 1000);
        assert_eq!(rows_bytes(&[vec![text.clone()], vec![text]]), 2 * row_bytes(&[Value::Text("x".repeat(1000))]));
        assert!(text_fits("repeat()", MAX_TEXT).is_ok());
        assert!(
            text_fits("repeat()", MAX_TEXT + 1)
                .unwrap_err()
                .to_string()
                .contains("repeat(): the result would be larger than 64 MiB")
        );
    }

    #[test]
    fn requests_share_what_all_of_them_may_hold() {
        let pool: &'static Pool =
            Box::leak(Box::new(Pool { held: AtomicUsize::new(0), cap: 4 << 20, first: AtomicU64::new(0) }));
        let a = Limits::make(8 << 20, None, pool, "SQL: ");
        let b = Limits::make(8 << 20, None, pool, "SQL: ");
        let c = Limits::make(8 << 20, None, pool, "SQL: ");
        let mut held_a = a.hold();
        held_a.add(3 << 20).unwrap();
        // The first request to find no room goes on past it, alone.
        let mut held_b = b.hold();
        held_b.add(2 << 20).unwrap();
        // Alone, `c` could hold this; beside them, it cannot.
        let mut held_c = c.hold();
        let busy = held_c.add(2 << 20).unwrap_err().to_string();
        assert!(busy.contains("try again") && busy.contains("(4 MiB)"), "{busy}");
        assert!(c.fits(2 << 20).is_err());
        assert_eq!(c.held(), 0);
        // What `b` and `a` no longer hold goes back to the others.
        drop((held_b, b));
        drop(held_a);
        held_c.add(2 << 20).unwrap();
        drop(a);
        drop(held_c);
        drop(c);
        assert_eq!(pool.held.load(Ordering::Acquire), 0);
        assert_eq!(pool.first.load(Ordering::Acquire), 0);
    }

    #[test]
    fn values_kept_are_counted_once_they_add_up() {
        let limits = Limits::new(1 << 20, None);
        let big = Value::Text("x".repeat(600 << 10));
        let mut live = limits.live();
        live.add(&Value::I64(1)).unwrap();
        assert_eq!(limits.held(), 0);
        live.add(&big).unwrap();
        assert!(limits.held() >= 600 << 10);
        let over = live.add(&big).unwrap_err().to_string();
        assert!(over.contains("more than 1 MiB"), "{over}");
        drop(live);
        assert_eq!(limits.held(), 0);
        // A text larger than what the statement at work may still hold is refused before it is built.
        let _at_work = limits.begin();
        assert!(text_fits("repeat()", 900 << 10).is_ok());
        let refused = text_fits("repeat()", 2 << 20).unwrap_err().to_string();
        assert!(refused.contains("more than 1 MiB"), "{refused}");
    }

    #[test]
    fn work_reads_the_clock() {
        let limits = Limits::new(1000, Some(Duration::from_millis(20)));
        let _at_work = limits.begin();
        std::thread::sleep(Duration::from_millis(30));
        assert!(limits.work(100).is_ok());
        assert!(limits.work(WORK_STEP).unwrap_err().to_string().contains("statement timeout"));
    }

    #[test]
    fn the_default_budget_is_a_share_of_the_memory() {
        let budget = default_memory();
        assert!((MEMORY_MIN..=MEMORY_MAX).contains(&budget));
        if let Some(total) = crate::memory::MemoryInfo::now().total_bytes {
            assert_eq!(budget, usize::try_from(total / 8).unwrap_or(MEMORY_MAX).clamp(MEMORY_MIN, MEMORY_MAX));
        }
    }
}
