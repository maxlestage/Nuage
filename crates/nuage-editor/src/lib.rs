//! Nuage DB Editor: a window to look after a Nuage DB server (on this
//! computer or online) without a terminal. It uses the server's management
//! API, with its management token: what `nuage apps` and `nuage settings`
//! do, and what the console's Settings page does.

pub mod address;
pub mod api;
pub mod app;
pub mod clip;
pub mod format;
pub mod i18n;
pub mod jobs;
pub mod local;
mod screens;
pub mod secret;
pub mod vault;

pub use app::{Editor, Prefs, SavedServer, Setup};

/// The name eframe keeps the editor's state under.
pub const APP_ID: &str = "nuage-editor";

impl eframe::App for Editor {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
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
