//! The settings in force at once (memory, limits, visibility), and what the
//! server read once at startup, shown and never changed here.

use eframe::egui::{self, RichText};

use super::{Doing, Notice, Outcome, Session, help};
use crate::address;
use crate::api::Settings;
use crate::format;
use crate::i18n::{Lang, t, tf};
use crate::theme::{self, Mark};

/// What a page that needs the settings says on a server without them.
pub const NO_SETTINGS: &str =
    "Update the server to see this here: its version has no settings for the editor (they came with Nuage DB 0.4.7).";

/// The settings, or what the page says instead: the server has none (an
/// older version), they failed, or they are on their way.
fn settings_or_why(ui: &mut egui::Ui, session: &Session, l: Lang) -> Option<Settings> {
    if !session.server.caps().settings {
        theme::status(ui, Mark::Warn, t(l, NO_SETTINGS));
        return None;
    }
    match (&session.settings, &session.settings_error) {
        (Some(settings), _) => {
            if settings.read_only {
                theme::status(
                    ui,
                    Mark::Warn,
                    t(
                        l,
                        "The settings of this server are those of its environment (NUAGE_SETTINGS=environment): the editor shows them and cannot change them.",
                    ),
                );
            }
            Some(settings.clone())
        }
        (None, Some(e)) => {
            theme::status(ui, Mark::Bad, e.explain(l));
            None
        }
        (None, None) => {
            help(ui, t(l, "Reading the settings…"));
            None
        }
    }
}

/// A setting's field, with where its value comes from and the button that gives it back.
fn setting_field(ui: &mut egui::Ui, session: &mut Session, l: Lang, key: &str, label: &str, help_text: &str) {
    let ctx = ui.ctx().clone();
    let read_only = session.settings.as_ref().is_some_and(|s| s.read_only);
    let setting = session.settings.as_ref().and_then(|s| s.get(key)).cloned();
    ui.add_space(6.0);
    let name = ui.strong(label);
    let field = session.fields.entry(key.to_owned()).or_default();
    ui.add_enabled(!read_only, egui::TextEdit::singleline(field).desired_width(180.0)).labelled_by(name.id);
    help(ui, help_text);
    let Some(s) = setting else { return };
    let source = match (s.source.as_str(), &s.environment) {
        ("saved", Some(env)) => tf(l, "Saved here: it wins over the environment ({}={}).", &[&s.variable, env]),
        ("saved", None) => tf(l, "Saved here ({} is not set in the environment).", &[&s.variable]),
        ("environment", Some(env)) => tf(l, "From the environment ({}={}).", &[&s.variable, env]),
        _ => tf(l, "Default ({} is not set).", &[&s.variable]),
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(source).weak());
        if s.source == "saved" && !read_only {
            let back = if s.environment.is_some() {
                t(l, "Back to the environment's value")
            } else {
                t(l, "Back to the default")
            };
            if ui.add_enabled(!session.busy(), egui::Button::new(back).small()).clicked() {
                let key = key.to_owned();
                session.act(&ctx, Doing::Settings, move |server| server.reset_setting(&key).map(Outcome::Settings));
            }
        }
    });
}

/// Sends the fields of `keys` that changed, in one call: all or none.
fn save_fields(ui: &egui::Ui, session: &mut Session, l: Lang, keys: &[&str]) {
    let Some(settings) = &session.settings else { return };
    let mut changes = Vec::new();
    for key in keys {
        let Some(typed) = session.fields.get(*key) else { continue };
        let Some(s) = settings.get(key) else { continue };
        let typed = typed.trim();
        // Left as it was shown: it stays where it comes from.
        let shown = s.shown();
        let same = typed.eq_ignore_ascii_case(&shown)
            || crate::api::mib_of(typed).zip(crate::api::mib_of(&shown)).is_some_and(|(a, b)| a == b);
        if same {
            continue;
        }
        changes.push(((*key).to_owned(), Some(typed.to_owned()).filter(|v| !v.is_empty())));
    }
    if changes.is_empty() {
        session.notice = Some(Notice::ok(t(l, "Nothing changed.")));
        return;
    }
    session.act(ui.ctx(), Doing::Settings, move |server| {
        let list: Vec<(&str, Option<String>)> = changes.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        server.save_settings(&list).map(Outcome::Settings)
    });
}

pub(super) fn memory(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    let Some(settings) = settings_or_why(ui, session, l) else { return };
    let m = &settings.memory;
    let process = session.status.as_ref().and_then(|s| s.memory.process_bytes);
    match (process, m.total_bytes, m.used_bytes) {
        (Some(process), Some(total), Some(used)) => {
            ui.label(tf(
                l,
                "Nuage DB uses {} now. This server has {}, {} of it free.",
                &[&format::gib(l, process), &format::gib(l, total), &format::gib(l, total.saturating_sub(used))],
            ));
        }
        (None, Some(total), Some(used)) => {
            ui.label(tf(
                l,
                "This server may use {}: {} used, {} free.",
                &[&format::bytes(l, total), &format::bytes(l, used), &format::bytes(l, total.saturating_sub(used))],
            ));
        }
        _ => {
            ui.label(t(l, "This system does not tell Nuage DB its memory: say how much it may count on."));
        }
    }
    help(
        ui,
        t(
            l,
            "Amounts are written auto, 512 MiB or 2 GiB (a number alone is in MiB). In force at once, with no restart.",
        ),
    );
    let detected = m.detected_bytes.map_or_else(|| t(l, "unknown").to_owned(), |d| format::bytes(l, d));
    setting_field(
        ui,
        session,
        l,
        "memory.total_mb",
        t(l, "Memory Nuage DB may count on"),
        &tf(
            l,
            "auto: what the system reports ({}), or NUAGE_MEMORY_MB when it is set. An amount leaves the rest to the other programs.",
            &[&detected],
        ),
    );
    let budget = m.cache_budget_bytes.map_or_else(|| t(l, "unknown").to_owned(), |b| format::bytes(l, b));
    let ceiling = m.cache_ceiling_bytes.map_or_else(|| budget.clone(), |b| format::bytes(l, b));
    setting_field(
        ui,
        session,
        l,
        "memory.cache_mb",
        t(l, "Page caches, all the applications together"),
        &tf(
            l,
            "auto follows the memory: a quarter of it at first, up to six tenths while there is room, less when it gets tight. An amount is a maximum. Now: {} (up to {}).",
            &[&budget, &ceiling],
        ),
    );
    setting_field(
        ui,
        session,
        l,
        "memory.kv_mb",
        t(l, "Key-value store of each application"),
        t(
            l,
            "auto: a sixteenth of the memory each, an eighth for all of them together. Lowering it drops the least recently used keys at once.",
        ),
    );
    ui.add_space(8.0);
    if ui.add_enabled(!session.busy() && !settings.read_only, theme::primary(t(l, "Save the memory"), ui)).clicked() {
        save_fields(ui, session, l, &["memory.total_mb", "memory.cache_mb", "memory.kv_mb"]);
    }

    ui.add_space(16.0);
    theme::subheading(ui, t(l, "Memory by application"));
    ui.label(t(
        l,
        "Each application shares the page caches of the server by use (auto), or has an amount set apart for it alone.",
    ));
    ui.add_space(6.0);
    let ctx = ui.ctx().clone();
    // One block per application: a table of six columns
    // would run past the right edge of a small window.
    for a in &settings.applications {
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong(&a.name);
            help(
                ui,
                tf(
                    l,
                    "Its cache holds {} of {} now.",
                    &[&format::bytes(l, a.cache_bytes), &format::bytes(l, a.cache_capacity_bytes)],
                ),
            );
            let (cache, kv) =
                session.app_fields.entry(a.name.clone()).or_insert_with(|| ("auto".into(), "auto".into()));
            // One line for each field, with its words: they never part, and fit a small window.
            for (what, field) in [(t(l, "Page cache"), cache), (t(l, "Key-value store"), kv)] {
                ui.horizontal(|ui| {
                    ui.label(what);
                    ui.name_inputs(format!("{} · {what}", a.name), |ui| {
                        ui.add_enabled(!settings.read_only, egui::TextEdit::singleline(field).desired_width(100.0))
                    });
                });
            }
            let (cache, kv) = session.app_fields.get(&a.name).cloned().unwrap_or_default();
            let label = format!("{} · {}", t(l, "Save"), a.name);
            if ui.add_enabled(!session.busy() && !settings.read_only, egui::Button::new(label)).clicked() {
                let name = a.name.clone();
                session.act(&ctx, Doing::Memory, move |server| {
                    server.set_memory(&name, Some(&cache), Some(&kv)).map(Outcome::Memory)
                });
            }
        });
        ui.add_space(4.0);
    }
    if !settings.ignored.is_empty() {
        ui.add_space(8.0);
        help(
            ui,
            tf(l, "{} setting(s) saved earlier are ignored while the environment rules.", &[&settings.ignored.len()]),
        );
    }
}

pub(super) fn limits(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    let Some(settings) = settings_or_why(ui, session, l) else { return };
    help(ui, t(l, "In force at once, with no restart."));
    setting_field(
        ui,
        session,
        l,
        "statement.memory_mb",
        t(l, "Memory of one statement"),
        t(
            l,
            "What one SQL statement may hold at once; past it, the statement fails with a clear error instead of filling the memory. auto: an eighth of the memory.",
        ),
    );
    setting_field(
        ui,
        session,
        l,
        "statement.seconds",
        t(l, "Seconds a statement may run"),
        t(l, "Past it, the statement stops with an error and its changes are undone. 0: no limit."),
    );
    setting_field(
        ui,
        session,
        l,
        "postgres.connections",
        t(l, "PostgreSQL connections at once"),
        t(l, "Each has a thread; the next ones are refused as PostgreSQL does. Between 1 and 10 000."),
    );
    ui.add_space(8.0);
    if ui.add_enabled(!session.busy() && !settings.read_only, theme::primary(t(l, "Save the limits"), ui)).clicked() {
        let connections = session.fields.get("postgres.connections").map(|v| v.trim().to_owned()).unwrap_or_default();
        match connections.parse::<u64>() {
            Ok(n) if !(1..=10_000).contains(&n) => {
                session.notice =
                    Some(Notice::bad(tf(l, "PostgreSQL connections at once: {} is not between 1 and 10 000.", &[&n])));
            }
            _ => save_fields(ui, session, l, &["statement.memory_mb", "statement.seconds", "postgres.connections"]),
        }
    }
    ui.add_space(16.0);
    theme::subheading(ui, t(l, "Visibility"));
    let before = session.public;
    ui.add_enabled(
        !settings.read_only && !session.busy(),
        egui::Checkbox::new(
            &mut session.public,
            t(l, "Public status: /api/status and the dashboard show a public subset without signing in"),
        ),
    );
    if session.public != before {
        let value = session.public.to_string();
        session.act(ui.ctx(), Doing::Settings, move |server| {
            server.save_settings(&[("status.public", Some(value))]).map(Outcome::Settings)
        });
    }
}

/// What the server read once, when it started: shown, not changed here.
pub(super) fn startup(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    if !session.server.caps().settings {
        theme::status(ui, Mark::Warn, t(l, "This version does not list them: the console's Server page does."));
        return;
    }
    let Some(settings) = settings_or_why(ui, session, l) else { return };
    ui.label(t(
        l,
        "The server reads these before it opens its data, or they are what it listens on and what opens it: a wrong value could lock you out, so they are not changed from here.",
    ));
    let heroku = address::normalize(session.server.address()).is_ok_and(|a| address::is_heroku(&a.host));
    let place = match (&settings.settings_file, heroku) {
        (Some(file), _) => {
            tf(l, "On the server's computer: its settings file {}, then a restart of its service.", &[file])
        }
        (None, true) => t(l, "On Heroku: Settings, then Config Vars; Heroku restarts the server by itself.").to_owned(),
        (None, false) => {
            t(l, "In the server's environment (a host's variables, Docker's .env), then a restart.").to_owned()
        }
    };
    theme::status(ui, Mark::Off, place);
    ui.add_space(8.0);
    let show_all_id = egui::Id::unique("startup-show-all");
    let mut show_all = ui.data(|d| d.get_temp::<bool>(show_all_id)).unwrap_or(false);
    // A line each, its value after the name or under it: a long path never runs past the window.
    for s in settings.startup.iter().filter(|s| show_all || s.value.is_some()) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&s.variable).monospace().strong());
            match s.value.as_deref() {
                Some("***") => ui.label(RichText::new(t(l, "set (a secret: not shown)")).weak()),
                Some(value) => ui.monospace(value),
                None => ui.label(RichText::new(t(l, "not set")).weak()),
            };
        });
    }
    if settings.startup.iter().any(|s| s.value.is_none())
        && ui.checkbox(&mut show_all, t(l, "Show the ones not set")).changed()
    {
        ui.data_mut(|d| d.insert_temp(show_all_id, show_all));
    }
}
