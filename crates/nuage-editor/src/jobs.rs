//! Calls to the server, and to the password store or the files of this
//! computer, run on a thread of their own: the window never waits for them,
//! and a call that panics (a bug) becomes an error shown in the window
//! instead of closing it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use crate::api::ApiError;

/// The most calls waiting at once: past it, a call fails at once (a server
/// that never answers must not pile threads up).
pub const MAX_CALLS: usize = 8;

/// The calls of one window, counted.
#[derive(Clone, Debug, Default)]
pub struct Calls {
    running: Arc<AtomicUsize>,
}

impl Calls {
    /// How many are waiting.
    pub fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }
}

/// Counts a call until its thread ends, however it ends.
struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A call in flight, and then its answer.
pub struct Job<T> {
    answer: Receiver<Result<T, ApiError>>,
    since: Instant,
}

impl<T> std::fmt::Debug for Job<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").field("since", &self.since).finish_non_exhaustive()
    }
}

impl<T: Send + 'static> Job<T> {
    /// Runs `work` on a thread; `ctx` is asked to paint again when it is done,
    /// so the answer shows at once.
    pub fn start(
        ctx: &eframe::egui::Context,
        calls: &Calls,
        work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
    ) -> Job<T> {
        let since = Instant::now();
        if calls.running.fetch_add(1, Ordering::SeqCst) >= MAX_CALLS {
            calls.running.fetch_sub(1, Ordering::SeqCst);
            return Job::done(Err(ApiError::Busy));
        }
        let running = Running(calls.running.clone());
        let (send, answer) = channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("nuage-editor-call".into()).spawn(move || {
            let _running = running;
            let result = crate::guard::catch(work).unwrap_or_else(|failure| Err(ApiError::Bug(failure.message)));
            let _ = send.send(result);
            ctx.request_repaint();
        });
        match spawned {
            Ok(_) => Job { answer, since },
            // No thread (its counter went with the closure): the answer is the error, at once.
            Err(e) => Job::done(Err(ApiError::Bug(format!("cannot start a call: {e}")))),
        }
    }

    /// A job whose answer is already there.
    pub fn done(result: Result<T, ApiError>) -> Job<T> {
        let (send, answer) = channel();
        let _ = send.send(result);
        Job { answer, since: Instant::now() }
    }

    /// The answer, once it is there (`None`: still waiting).
    pub fn poll(&self) -> Option<Result<T, ApiError>> {
        match self.answer.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(ApiError::Bug("the call stopped without an answer".into()))),
        }
    }

    /// How long it has been waiting.
    pub fn elapsed(&self) -> Duration {
        self.since.elapsed()
    }
}

/// Takes the answer of the job in `slot` when it is there, and empties the slot.
pub fn take<T: Send + 'static>(slot: &mut Option<Job<T>>) -> Option<Result<T, ApiError>> {
    let answer = slot.as_ref()?.poll()?;
    *slot = None;
    Some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait<T: Send + 'static>(job: &Job<T>) -> Result<T, ApiError> {
        let started = Instant::now();
        loop {
            if let Some(answer) = job.poll() {
                return answer;
            }
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_bug_becomes_an_error_and_calls_are_capped() {
        let ctx = eframe::egui::Context::default();
        let calls = Calls::default();
        let job = Job::<()>::start(&ctx, &calls, || panic!("made-up bug"));
        assert_eq!(wait(&job), Err(ApiError::Bug("made-up bug".into())));
        // Calls that never end hold their places; the next one is refused at once.
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Arc::new(std::sync::Mutex::new(gate));
        let held: Vec<_> = (0..MAX_CALLS)
            .map(|_| {
                let gate = gate.clone();
                Job::start(&ctx, &calls, move || {
                    let _ = gate.lock().unwrap().recv();
                    Ok(())
                })
            })
            .collect();
        assert_eq!(Job::<()>::start(&ctx, &calls, || Ok(())).poll(), Some(Err(ApiError::Busy)));
        for _ in 0..MAX_CALLS {
            release.send(()).unwrap();
        }
        for job in &held {
            assert_eq!(wait(job), Ok(()));
        }
        let started = Instant::now();
        while calls.running() > 0 {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(wait(&Job::start(&ctx, &calls, || Ok(7))), Ok(7));
    }
}
