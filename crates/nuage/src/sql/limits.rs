//! What a statement may use: memory and time.
//!
//! A statement holds rows in memory while it works (what it read, joined,
//! grouped, sorted, and its answer): nothing else of the server grows with
//! what one request asks for. Those rows are counted, by an estimate of their
//! size, against a budget; past it the statement fails instead of taking the
//! memory of the whole server. The statements of one request share the budget:
//! their answers are all held until the request is answered.
//!
//! A statement also has a deadline, read in the loops that run once per row:
//! past it, it fails instead of keeping its thread.
//!
//! Both come from two settings of the process ([`set_statement_memory`],
//! [`set_statement_timeout`]), read when a request starts.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use memoire_orm::{Error, Result, Value};
use serde_json::Value as Json;

/// The largest text (or JSON array) a function or an operator may build:
/// `repeat`, `lpad`, `||`, `string_agg`... are asked for sizes, and the size
/// is checked before the memory is taken.
pub(crate) const MAX_TEXT: usize = 64 << 20;

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

/// An eighth of the memory of the server, read once: a few statements at the
/// same time, the page caches and the rest all fit.
fn default_memory() -> usize {
    static DEFAULT: OnceLock<usize> = OnceLock::new();
    *DEFAULT.get_or_init(|| match crate::memory::MemoryInfo::now().total_bytes {
        Some(total) => usize::try_from(total / 8).unwrap_or(MEMORY_MAX).clamp(MEMORY_MIN, MEMORY_MAX),
        None => MEMORY_UNKNOWN,
    })
}

/// The budget and the deadline of the statements of one request.
#[derive(Debug)]
pub(crate) struct Limits {
    /// Bytes the statements may hold at once.
    memory: usize,
    /// Bytes held now.
    held: Cell<usize>,
    /// How long each statement may run.
    timeout: Option<Duration>,
    /// When the statement at work must have ended.
    deadline: Cell<Option<Instant>>,
    /// Steps since the clock was last read.
    steps: Cell<u32>,
    /// The engine left a join to the SQL layer (see [`Limits::decline`]).
    declined: Cell<bool>,
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
    /// The limits of a request that starts now, from the settings.
    pub(crate) fn from_settings() -> Rc<Limits> {
        Limits::new(statement_memory(), statement_timeout())
    }

    pub(crate) fn new(memory: usize, timeout: Option<Duration>) -> Rc<Limits> {
        Rc::new(Limits {
            memory,
            held: Cell::new(0),
            timeout,
            deadline: Cell::new(None),
            steps: Cell::new(0),
            declined: Cell::new(false),
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
            "SQL: the statement needs more than {} MiB of memory: add a LIMIT or a filter, or raise NUAGE_STATEMENT_MB",
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
        self.held.set(held);
        Ok(())
    }

    /// `bytes` counted before are no longer held.
    #[inline]
    pub(crate) fn release(&self, bytes: usize) {
        self.held.set(self.held.get().saturating_sub(bytes));
    }

    /// Fails when `bytes` more would not fit, without counting them: for the
    /// engine's scans, whose rows are counted by the SQL layer once it has them.
    pub(crate) fn fits(&self, bytes: usize) -> Result<()> {
        if self.held.get().saturating_add(bytes) > self.memory { Err(self.out_of_memory()) } else { Ok(()) }
    }

    /// Bytes held now.
    pub(crate) fn held(&self) -> usize {
        self.held.get()
    }

    /// A statement ended: `bytes` are held (the answers so far).
    pub(crate) fn settle(&self, bytes: usize) {
        self.held.set(bytes);
    }

    /// One step of a loop over rows: every so many, the clock is read.
    #[inline]
    pub(crate) fn tick(&self) -> Result<()> {
        let n = self.steps.get().wrapping_add(1);
        self.steps.set(n);
        if n.is_multiple_of(STEPS) { self.check_time() } else { Ok(()) }
    }

    /// Fails when the statement ran past its deadline.
    pub(crate) fn check_time(&self) -> Result<()> {
        match self.deadline.get() {
            Some(deadline) if Instant::now() >= deadline => {
                let limit = self.timeout.unwrap_or_default();
                Err(Error::Validation(format!(
                    "SQL: statement timeout: it ran for more than {} (NUAGE_STATEMENT_SECONDS)",
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

fn json_bytes(j: &Json) -> usize {
    match j {
        Json::String(s) => s.len(),
        Json::Array(items) => items.iter().map(|x| size_of::<Json>() + json_bytes(x)).sum(),
        Json::Object(map) => map.iter().map(|(k, x)| 2 * size_of::<Json>() + k.len() + json_bytes(x)).sum(),
        _ => 0,
    }
}

/// About what a value takes in memory.
#[inline]
pub(crate) fn value_bytes(v: &Value) -> usize {
    size_of::<Value>()
        + match v {
            Value::Text(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Json(j) => json_bytes(j),
            _ => 0,
        }
}

/// About what a row takes in memory.
#[inline]
pub(crate) fn row_bytes(row: &[Value]) -> usize {
    size_of::<Vec<Value>>() + row.iter().map(value_bytes).sum::<usize>()
}

/// About what rows take in memory.
pub(crate) fn rows_bytes(rows: &[Vec<Value>]) -> usize {
    rows.iter().map(|r| row_bytes(r)).sum()
}

/// The error of a function or an operator asked for a text larger than
/// [`MAX_TEXT`]: `what` names it (`repeat()`, `||`).
pub(crate) fn text_fits(what: &str, bytes: usize) -> Result<()> {
    if bytes > MAX_TEXT {
        return Err(Error::Conversion(format!("{what}: the result would be larger than {} MiB", MAX_TEXT >> 20)));
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
    fn the_default_budget_is_a_share_of_the_memory() {
        let budget = default_memory();
        assert!((MEMORY_MIN..=MEMORY_MAX).contains(&budget));
        if let Some(total) = crate::memory::MemoryInfo::now().total_bytes {
            assert_eq!(budget, usize::try_from(total / 8).unwrap_or(MEMORY_MAX).clamp(MEMORY_MIN, MEMORY_MAX));
        }
    }
}
