//! A bug in the window's code must not close the window: the frame that
//! panicked is dropped, and the window shows a page that says so, with what
//! failed and a way back to the start. A call that panics on its thread
//! becomes an error the same way (see [`crate::jobs`]).
//!
//! The panic hook keeps the last panic's message and place, for that page,
//! and prints them on the terminal when there is one. Nothing is written to
//! a file: the editor's log file is for the windows that could not open
//! (see `main.rs`).

use std::any::Any;
use std::cell::RefCell;
use std::io::Write as _;
use std::panic::{AssertUnwindSafe, PanicHookInfo};

#[cfg(not(panic = "unwind"))]
compile_error!("Nuage DB Editor needs panic = \"unwind\": a bug shows a page instead of closing the window");

/// A bug that was caught.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The panic's message.
    pub message: String,
    /// Where in the code (`file:line`), when the hook saw it.
    pub place: Option<String>,
}

impl Failure {
    /// What the person can copy and send with a report. The editor's secrets print as
    /// `***`, and the message is [`redact`]ed (a panic of a dependency prints raw text).
    pub fn details(&self) -> String {
        let place = self.place.as_deref().unwrap_or("?");
        format!(
            "nuage-editor {} ({}, {}): {} at {place}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.message
        )
    }
}

/// The shortest run of letters, digits, `-` and `_` taken for a secret.
const SECRET_RUN: usize = 24;

/// `text` with what may be a secret hidden: each run of at least
/// [`SECRET_RUN`] letters, digits, `-` or `_` (a token, a key) becomes `***`.
/// A panic raised in a dependency prints its text as it is: egui's text
/// editing, for one, quotes the string it failed on, which may be a token.
pub fn redact(text: &str) -> String {
    fn flush(run: &mut String, out: &mut String) {
        if run.chars().count() >= SECRET_RUN {
            out.push_str("***");
        } else {
            out.push_str(run);
        }
        run.clear();
    }
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// The last panic the hook saw on a thread: its message and place.
struct Seen {
    message: String,
    place: Option<String>,
}

thread_local! {
    // Per thread: a call panicking on its own thread never takes the place of the window's.
    static LAST: RefCell<Option<Seen>> = const { RefCell::new(None) };
}

/// The text of a panic's payload (`panic!("…")` gives a `&str` or a `String`).
pub fn message_of(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_owned())
}

/// Keeps every panic's message and place for [`catch`], and says them on
/// standard error (debug builds and terminals see it; a double-click does not).
pub fn install_hook() {
    std::panic::set_hook(Box::new(record));
}

fn record(info: &PanicHookInfo<'_>) {
    let message = message_of(info.payload());
    let place = info.location().map(|l| format!("{}:{}", l.file(), l.line()));
    // Not eprintln!: it panics when standard error is gone, and a panic in the hook aborts.
    let _ = writeln!(
        std::io::stderr(),
        "nuage-editor: a bug: {} (at {})",
        redact(&message),
        place.as_deref().unwrap_or("?")
    );
    // Never panic in a panic hook: a thread being torn down only loses the place.
    let _ = LAST.try_with(|last| {
        if let Ok(mut last) = last.try_borrow_mut() {
            *last = Some(Seen { message, place });
        }
    });
}

/// Runs `f`; a panic in it comes back as the [`Failure`] it was, instead of
/// unwinding further (through the window system, which would close the window,
/// or abort on macOS).
pub fn catch<R>(f: impl FnOnce() -> R) -> Result<R, Failure> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|payload| failure_of(&*payload))
}

/// The failure a caught panic's payload says, with its place when the hook saw it on this thread.
pub fn failure_of(payload: &(dyn Any + Send)) -> Failure {
    let message = message_of(payload);
    let seen = LAST.try_with(|last| last.try_borrow_mut().ok().and_then(|mut last| last.take())).ok().flatten();
    let place = seen.filter(|seen| seen.message == message).and_then(|seen| seen.place);
    // What the window shows, and the details a person may paste in a public report.
    Failure { message: redact(&message), place }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_a_failure() {
        assert_eq!(catch(|| 7), Ok(7));
        let failure = catch(|| -> u8 { panic!("made-up bug {}", 42) }).unwrap_err();
        assert_eq!(failure.message, "made-up bug 42");
        let failure = catch(|| std::panic::panic_any(3_u8)).unwrap_err();
        assert_eq!(failure.message, "a panic without a message");
        assert!(failure.details().contains(env!("CARGO_PKG_VERSION")), "{}", failure.details());
        // A dependency's panic quoting a token: the token is not in what is shown or copied.
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let failure = catch(|| -> u8 { panic!("byte index 3 is not a char boundary; it is inside 'é' of `{token}`") })
            .unwrap_err();
        assert!(!failure.details().contains(token) && failure.message.contains("of `***`"), "{failure:?}");
        assert_eq!(redact("nuage_abcdefghijklmnopqrstuvwxyz0123 at src/app/mod.rs:12"), "*** at src/app/mod.rs:12");
        assert_eq!(redact("made-up bug 42"), "made-up bug 42");
    }

    #[test]
    fn the_hook_keeps_the_place() {
        // The hook is the process's: set it, use it, put the default one back.
        install_hook();
        let failure = catch(|| -> u8 { panic!("made-up bug with a place") });
        let _ = std::panic::take_hook();
        let failure = failure.unwrap_err();
        assert_eq!(failure.message, "made-up bug with a place");
        assert!(failure.place.as_deref().is_some_and(|p| p.contains("guard.rs")), "{failure:?}");
    }
}
