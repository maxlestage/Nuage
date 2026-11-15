//! What the window tests share: the window, driven headless through its
//! accessibility tree (labels, clicks, typing).
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use nuage_editor::clip::FakeSink;
use nuage_editor::vault::Vault;
use nuage_editor::{Editor, Prefs, Setup};

/// The window of an editor with `prefs`, its tokens kept in `vault`, its copies in `clipboard`.
pub fn window(prefs: Prefs, vault: Arc<dyn Vault>, clipboard: FakeSink) -> Harness<'static, Editor> {
    let editor = Editor::new(prefs, Setup::with(vault, Box::new(clipboard)));
    let mut harness = Harness::builder()
        .with_size([1100.0, 1600.0])
        // A new window (a dialog) settles in a few frames: its widgets are styled once they exist.
        .with_max_steps(30)
        .build_ui_state(|ui, editor: &mut Editor| editor.show(ui), editor);
    harness.run();
    harness
}

/// Steps the window until `done` holds (calls run on threads of their own).
pub fn wait_for(harness: &mut Harness<'_, Editor>, what: &str, done: impl Fn(&Harness<'_, Editor>) -> bool) {
    let started = Instant::now();
    while !done(harness) {
        assert!(started.elapsed() < Duration::from_secs(20), "waited too long for: {what}");
        std::thread::sleep(Duration::from_millis(20));
        harness.step();
    }
    harness.run();
}

pub fn has(harness: &Harness<'_, Editor>, label: &str) -> bool {
    harness.query_all_by_label_contains(label).next().is_some()
}

/// Whether `text` is anywhere in what the window shows or tells a screen reader.
pub fn shows_anywhere(harness: &Harness<'_, Editor>, text: &str) -> bool {
    harness.root().children_recursive().any(|node| {
        let node = node.accesskit_node();
        [node.label(), node.value(), node.description()].into_iter().flatten().any(|s| s.contains(text))
    })
}

/// Types `text` in the field named `label`, in place of what it holds.
pub fn type_into(harness: &mut Harness<'_, Editor>, label: &str, text: &str) {
    harness.get_by_label(label).click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    harness.run();
    harness.get_by_label(label).type_text(text);
    harness.run();
}

/// Clicks the button named `label` (a text of the same words may be elsewhere on the page).
pub fn click(harness: &mut Harness<'_, Editor>, label: &str) {
    harness.get_by_role_and_label(Role::Button, label).click();
    harness.run();
}
