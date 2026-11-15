//! Nuage DB Editor: a window to look after a Nuage DB server (on this
//! computer or online) without a terminal. It uses the server's management
//! API, with its management token: what `nuage apps` and `nuage settings`
//! do, and what the console's Settings page does.

pub mod address;
pub mod api;
pub mod app;
pub mod clip;
pub mod format;
pub mod guard;
pub mod i18n;
pub mod jobs;
pub mod local;
mod screens;
pub mod secret;
pub mod selftest;
pub mod vault;

pub use app::{Editor, Prefs, SavedServer, Setup};

/// The editor's name for the systems: the window's app id (Wayland and X11
/// group the window under the `.desktop` entry of that name), the macOS
/// bundle's identifier, and the password store's service (packaging/).
pub const APP_ID: &str = "io.github.maxlestage.nuage-editor";

/// The folder eframe keeps the editor's preferences in (`app.ron`), that of
/// the first versions: what they remembered stays.
pub const STORAGE_ID: &str = "nuage-editor";

impl eframe::App for Editor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.prefs);
    }

    /// Only the preferences are written: no state of a widget (a field's
    /// text), whatever it held.
    fn persist_egui_memory(&self) -> bool {
        false
    }
}
