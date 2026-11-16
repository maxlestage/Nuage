//! Copying a secret: the clipboard history (Windows' history and cloud
//! clipboard, macOS clipboard managers, KDE's Klipper) is asked to skip it.
//! Where the system's clipboard cannot be opened, egui copies it, and the
//! window says the history may keep it.

use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui;

use crate::secret::Secret;

/// How a copy went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Copied {
    /// Copied, with the history asked to skip it.
    Private,
    /// Copied the ordinary way (the reason the private way failed): the
    /// history may keep it.
    Plain(String),
}

/// Where secrets are copied to.
pub trait CopySink {
    fn copy_secret(&mut self, ctx: &egui::Context, text: &str) -> Copied;
    /// Empties the clipboard (after a secret was pasted where it goes).
    fn clear(&mut self, ctx: &egui::Context);
}

/// The system's clipboard, through arboard. One clipboard lives as long as
/// the window: on X11 the program that copied serves what it copied.
#[derive(Default)]
pub struct ArboardSink {
    clipboard: Option<arboard::Clipboard>,
}

impl CopySink for ArboardSink {
    fn copy_secret(&mut self, ctx: &egui::Context, text: &str) -> Copied {
        let reason = match self.private_copy(text) {
            Ok(()) => return Copied::Private,
            Err(e) => e.to_string(),
        };
        ctx.copy_text(text.to_owned());
        Copied::Plain(reason)
    }

    fn clear(&mut self, ctx: &egui::Context) {
        let cleared = self.clipboard.as_mut().is_some_and(|c| c.clear().is_ok());
        if !cleared {
            ctx.copy_text(String::new());
        }
    }
}

impl ArboardSink {
    fn private_copy(&mut self, text: &str) -> Result<(), arboard::Error> {
        let clipboard = match &mut self.clipboard {
            Some(clipboard) => clipboard,
            empty => empty.insert(arboard::Clipboard::new()?),
        };
        let set = clipboard.set();
        #[cfg(windows)]
        let set = {
            use arboard::SetExtWindows as _;
            set.exclude_from_history().exclude_from_cloud()
        };
        #[cfg(target_os = "macos")]
        let set = {
            use arboard::SetExtApple as _;
            set.exclude_from_history()
        };
        #[cfg(all(unix, not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))))]
        let set = {
            use arboard::SetExtLinux as _;
            set.exclude_from_history()
        };
        set.text(text)
    }
}

/// What the tests' clipboard received.
#[derive(Clone, Default)]
pub struct FakeSink {
    copies: Arc<Mutex<Vec<Secret>>>,
}

impl FakeSink {
    /// The texts copied, in order.
    pub fn copies(&self) -> Vec<String> {
        self.copies.lock().unwrap_or_else(PoisonError::into_inner).iter().map(|s| s.expose().to_owned()).collect()
    }
}

impl CopySink for FakeSink {
    fn copy_secret(&mut self, _ctx: &egui::Context, text: &str) -> Copied {
        self.copies.lock().unwrap_or_else(PoisonError::into_inner).push(Secret::new(text.to_owned()));
        Copied::Private
    }

    /// Recorded as an empty copy.
    fn clear(&mut self, _ctx: &egui::Context) {
        self.copies.lock().unwrap_or_else(PoisonError::into_inner).push(Secret::new(String::new()));
    }
}
