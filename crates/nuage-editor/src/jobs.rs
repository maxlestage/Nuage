//! Calls to the server run on a thread of their own: the window never waits
//! for the network, and a call that panics (a bug) becomes an error shown in
//! the window instead of closing it.

use std::sync::mpsc::{Receiver, TryRecvError, channel};

use crate::api::ApiError;

/// A call in flight, and then its answer.
pub struct Job<T> {
    answer: Receiver<Result<T, ApiError>>,
}

impl<T: Send + 'static> Job<T> {
    /// Runs `work` on a thread; `ctx` is asked to paint again when it is done,
    /// so the answer shows at once.
    pub fn start(ctx: &eframe::egui::Context, work: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Job<T> {
        let (send, answer) = channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("nuage-editor-call".into()).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err(ApiError::Failed("the editor failed unexpectedly (a bug): try again".into())));
            let _ = send.send(result);
            ctx.request_repaint();
        });
        if let Err(e) = spawned {
            // No thread: the answer is the error, at once.
            let (send, answer) = channel();
            let _ = send.send(Err(ApiError::Failed(format!("cannot start a call: {e}"))));
            return Job { answer };
        }
        Job { answer }
    }

    /// The answer, once it is there (`None`: still waiting).
    pub fn poll(&self) -> Option<Result<T, ApiError>> {
        match self.answer.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(ApiError::Failed("the call stopped without an answer".into()))),
        }
    }
}

/// Takes the answer of the job in `slot` when it is there, and empties the slot.
pub fn take<T: Send + 'static>(slot: &mut Option<Job<T>>) -> Option<Result<T, ApiError>> {
    let answer = slot.as_ref()?.poll()?;
    *slot = None;
    Some(answer)
}
