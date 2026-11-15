//! The pages that only show: the overview of a server, and what it was
//! started with.

use eframe::egui::{self, RichText};

use crate::api::{ApiError, Application, Caps, Settings};
use crate::format;
use crate::i18n::{Lang, t, tf};

/// What a page that needs the settings says on a server without them.
pub const NO_SETTINGS: &str =
    "Update the server to see this here: its version has no settings for the editor (they came with Nuage DB 0.4.7).";

/// What the overview shows.
pub struct View<'a> {
    pub address: &'a str,
    pub caps: &'a Caps,
    pub settings: Option<&'a Settings>,
    pub settings_error: Option<&'a ApiError>,
    pub apps: Option<&'a [Application]>,
}

/// The overview: is it there, how much memory, how many applications, and
/// anything its owner should know.
pub fn overview(ui: &mut egui::Ui, v: View<'_>, l: Lang) {
    ui.label(tf(l, "The server at {} answers, and its management token is right.", &[&v.address]));
    if let Some(version) = &v.caps.version {
        ui.label(RichText::new(tf(l, "Nuage DB {}", &[version])).weak());
    }
    ui.add_space(8.0);
    if !v.caps.settings {
        // A server deployed before the settings: applications and tokens only.
        egui::Grid::new("overview").num_columns(2).spacing([18.0, 8.0]).striped(true).show(ui, |ui| {
            ui.label(t(l, "Applications"));
            match v.apps {
                Some(apps) => ui.label(apps.len().to_string()),
                None => ui.spinner(),
            };
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(t(
                l,
                "This server (before the settings came, 0.4.7) lets the editor manage applications and tokens only: update it for memory and limits.",
            ))
            .color(ui.visuals().warn_fg_color),
        );
        return;
    }
    let Some(settings) = v.settings else {
        match v.settings_error {
            Some(e) => ui.label(RichText::new(e.explain(l)).color(ui.visuals().error_fg_color)),
            None => ui.spinner(),
        };
        return;
    };
    egui::Grid::new("overview").num_columns(2).spacing([18.0, 8.0]).striped(true).show(ui, |ui| {
        let m = &settings.memory;
        ui.label(t(l, "Memory"));
        match (m.total_bytes, m.used_bytes) {
            (Some(total), Some(used)) => ui.label(tf(
                l,
                "{} used, {} free of {}",
                &[&format::bytes(l, used), &format::bytes(l, total.saturating_sub(used)), &format::bytes(l, total)],
            )),
            _ => ui.label(t(l, "unknown")),
        };
        ui.end_row();
        ui.label(t(l, "Page caches"));
        match (m.cache_budget_bytes, m.cache_ceiling_bytes) {
            (Some(b), Some(c)) => ui.label(tf(
                l,
                "{} for all the applications (up to {} while memory has room)",
                &[&format::bytes(l, b), &format::bytes(l, c)],
            )),
            (Some(b), None) => ui.label(format::bytes(l, b)),
            _ => ui.label(t(l, "unknown")),
        };
        ui.end_row();
        ui.label(t(l, "Applications"));
        match v.apps {
            Some(apps) => ui.label(apps.len().to_string()),
            None => ui.spinner(),
        };
        ui.end_row();
        let backup = settings
            .startup
            .iter()
            .find(|s| s.value.is_some() && matches!(s.variable.as_str(), "NUAGE_BACKUP_DIR" | "NUAGE_BACKUP_S3_BUCKET"))
            .and_then(|s| s.value.clone());
        ui.label(t(l, "Backups"));
        match backup {
            Some(target) => ui.label(target),
            None => ui.label(
                RichText::new(t(l, "none: if the disk of this server is lost, its data is lost"))
                    .color(ui.visuals().warn_fg_color),
            ),
        };
        ui.end_row();
    });
    ui.add_space(8.0);
    if settings.read_only {
        ui.label(
            RichText::new(t(
                l,
                "The settings of this server are those of its environment (NUAGE_SETTINGS=environment): the editor shows them, and changes nothing.",
            ))
            .color(ui.visuals().warn_fg_color),
        );
    }
    if !settings.ignored.is_empty() {
        ui.label(tf(
            l,
            "{} setting(s) saved earlier are ignored while the environment rules.",
            &[&settings.ignored.len()],
        ));
    }
}

/// What the server read once, when it started: shown, not changed here.
pub fn startup(ui: &mut egui::Ui, settings: Option<&Settings>, l: Lang) {
    let Some(settings) = settings else {
        ui.spinner();
        return;
    };
    ui.label(t(
        l,
        "The server reads these before it opens its data, or they are what it listens on and what opens it: a wrong value could lock you out, so they are not changed from here. They change in the settings file (or the host's variables), then the server restarts.",
    ));
    if let Some(file) = &settings.settings_file {
        ui.label(tf(l, "Its settings file: {}", &[file]));
    }
    ui.add_space(8.0);
    egui::Grid::new("startup").num_columns(2).striped(true).spacing([18.0, 4.0]).show(ui, |ui| {
        ui.strong(t(l, "Variable"));
        ui.strong(t(l, "Value"));
        ui.end_row();
        for s in settings.startup.iter().filter(|s| s.value.is_some()) {
            ui.monospace(&s.variable);
            match s.value.as_deref() {
                Some("***") => ui.label(RichText::new(t(l, "set (a secret: not shown)")).weak()),
                Some(value) => ui.monospace(value),
                None => ui.label(""),
            };
            ui.end_row();
        }
    });
}
