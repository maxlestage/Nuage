//! The overview: does the server work, how many applications and when they
//! last connected, how much memory it uses, and whether its backups work.
//! The answers come from `/api/status` when the management token opens it
//! (0.4.7 and later); an older server is told apart, and the page says what
//! updating it would show.

use std::collections::BTreeMap;

use eframe::egui::{self, RichText};

use super::{Here, Section, Session, help};
use crate::address;
use crate::api::{LastConnection, Settings, Status};
use crate::format;
use crate::i18n::{Lang, t, tf};
use crate::theme::{self, Mark};

pub(super) fn page(ui: &mut egui::Ui, session: &mut Session, here: &Here, l: Lang) {
    let _ = here;
    let now = format::now_ms();
    let heroku = address::normalize(session.server.address()).is_ok_and(|a| address::is_heroku(&a.host));
    let status = session.status.clone();
    let settings = session.settings.clone();
    let caps = session.server.caps().clone();
    let stale = session.stale.is_some();
    let mut go = None;
    ui.scope(|ui| {
        if stale {
            // What could not be read again stays, greyed.
            ui.multiply_opacity(0.55);
        }
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::mark(ui, Mark::Ok);
                theme::subheading(ui, t(l, "Nuage DB answers"));
            });
            match &status {
                Some(s) => {
                    ui.label(answers(l, s));
                }
                None => {
                    ui.label(tf(
                        l,
                        "The server at {} answers, and its management token is right.",
                        &[&session.server.address()],
                    ));
                    help(
                        ui,
                        if caps.status {
                            // It has a status: the last read of it failed on the way.
                            t(l, "Its version, its memory and its backups could not be read this time: Refresh asks again.")
                        } else {
                            t(
                                l,
                                "Newer versions of Nuage DB tell the editor their version, their memory and whether the last backup worked: update the server to see it here.",
                            )
                        },
                    );
                }
            }
        });
        ui.add_space(8.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Applications"));
            let count = session.apps.as_ref().map_or(0, Vec::len) as u64;
            ui.label(applications(l, count, status.as_ref(), &session.sites, now));
            if ui.button(t(l, "See them")).clicked() {
                go = Some(Section::Applications);
            }
        });
        ui.add_space(8.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Memory"));
            match memory(l, status.as_ref(), settings.as_ref()) {
                Some(line) => ui.label(line),
                None => ui.label(RichText::new(t(l, "This server does not say its memory to the editor.")).weak()),
            };
            if caps.settings && ui.button(t(l, "Change")).clicked() {
                go = Some(Section::Memory);
            }
        });
        ui.add_space(8.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Backups"));
            if status.is_none() && caps.status {
                help(ui, t(l, "Not read this time: Refresh asks again."));
            } else {
                for (m, line) in backups(l, status.as_ref(), settings.as_ref(), heroku, now) {
                    theme::status(ui, m, line);
                }
            }
            egui::CollapsingHeader::new(t(l, "How to change where they go")).id_salt("backup-where").show(ui, |ui| {
                ui.label(t(
                    l,
                    "The destination is chosen when Nuage DB is installed: run the installer again with another destination (the guide shows the line). On Heroku: Settings, then Config Vars, the NUAGE_BACKUP_S3_* variables. The server restarts and keeps its data.",
                ));
                if ui.button(t(l, "Open the guide")).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(super::start::GUIDE));
                }
            });
        });
    });
    ui.add_space(8.0);
    if !caps.settings {
        theme::status(
            ui,
            Mark::Warn,
            t(
                l,
                "This server (before the settings came, 0.4.7) lets the editor manage applications and tokens only: update it for memory and limits.",
            ),
        );
    }
    if let Some(settings) = &settings {
        if settings.read_only {
            theme::status(
                ui,
                Mark::Warn,
                t(
                    l,
                    "The settings of this server are those of its environment (NUAGE_SETTINGS=environment): the editor shows them, and changes nothing.",
                ),
            );
        }
        if !settings.ignored.is_empty() {
            help(
                ui,
                tf(
                    l,
                    "{} setting(s) saved earlier are ignored while the environment rules.",
                    &[&settings.ignored.len()],
                ),
            );
        }
    }
    if let Some((_, at)) = session.updated {
        help(ui, tf(l, "Updated {}", &[&format::ago(l, at, now)]));
    }
    if let Some(section) = go {
        session.section = section;
        session.open_app = None;
        session.notice = None;
    }
}

/// "running for 3 days · version 0.4.7 · everything it stores is encrypted".
fn answers(l: Lang, s: &Status) -> String {
    let mut parts = vec![tf(l, "running for {}", &[&format::span(l, s.uptime_s)]), tf(l, "version {}", &[&s.version])];
    if s.encryption.enabled {
        // The ciphers' names say nothing to the people this is for.
        parts.push(t(l, "everything it stores is encrypted").to_owned());
    } else {
        parts.push(t(l, "its data is not encrypted on its disk").to_owned());
    }
    parts.join(" · ")
}

/// "3 applications · last connection 2 min ago (shop)". A test from the
/// editor is not a connection of a site: the one before it counts (`sites`:
/// the last ones the editor saw that were not its own).
fn applications(
    l: Lang,
    count: u64,
    status: Option<&Status>,
    sites: &BTreeMap<String, LastConnection>,
    now: i64,
) -> String {
    let apps = if format::singular(l, count) {
        tf(l, "{} application", &[&count])
    } else {
        tf(l, "{} applications", &[&count])
    };
    let last = status.and_then(|s| {
        s.databases
            .iter()
            .filter_map(|d| {
                let c = d.last_connection.as_ref().filter(|c| !c.by_the_editor()).or_else(|| sites.get(&d.name))?;
                Some((format::parse_time(&c.at)?, d.name.as_str()))
            })
            .max()
    });
    match last {
        Some((at, name)) => tf(l, "{} · last connection {} ({})", &[&apps, &format::ago(l, at, now), &name]),
        None => apps,
    }
}

/// What the server uses, and has: from its status, else from its settings.
fn memory(l: Lang, status: Option<&Status>, settings: Option<&Settings>) -> Option<String> {
    if let Some(m) = status.map(|s| &s.memory)
        && let (Some(used), Some(total), Some(free)) = (m.process_bytes, m.total_bytes, m.available_bytes)
    {
        return Some(tf(
            l,
            "Nuage DB uses {} · this server has {}, {} free",
            &[&format::gib(l, used), &format::gib(l, total), &format::gib(l, free)],
        ));
    }
    let m = &settings?.memory;
    let (total, used) = (m.total_bytes?, m.used_bytes?);
    Some(tf(
        l,
        "{} used, {} free of {}",
        &[&format::gib(l, used), &format::gib(l, total.saturating_sub(used)), &format::gib(l, total)],
    ))
}

/// Whether the backups work, each line with its mark.
fn backups(
    l: Lang,
    status: Option<&Status>,
    settings: Option<&Settings>,
    heroku: bool,
    now: i64,
) -> Vec<(Mark, String)> {
    let none = || {
        let mut lines = vec![(Mark::Bad, t(l, "No backup: if this server's disk fails, its data is lost.").to_owned())];
        if heroku {
            lines.push((
                Mark::Bad,
                t(l, "On Heroku the disk is emptied at every restart, at least once a day.").to_owned(),
            ));
        }
        lines
    };
    let Some(status) = status else {
        // An older server: where they go, from what it was started with.
        let target = settings.and_then(|s| {
            s.startup
                .iter()
                .find(|v| {
                    v.value.is_some() && matches!(v.variable.as_str(), "NUAGE_BACKUP_DIR" | "NUAGE_BACKUP_S3_BUCKET")
                })
                .and_then(|v| v.value.clone())
        });
        let mut lines = match (target, settings) {
            (Some(target), _) => vec![(Mark::Off, tf(l, "Backups go to {}.", &[&target]))],
            (None, Some(_)) => none(),
            (None, None) => Vec::new(),
        };
        lines.push((
            Mark::Off,
            t(
                l,
                "Newer versions of Nuage DB tell the editor whether the last copy worked: update the server to see it here.",
            )
            .to_owned(),
        ));
        return lines;
    };
    let b = &status.backup;
    if !b.enabled {
        return none();
    }
    let target = b.target.clone().unwrap_or_default();
    if !b.last_ok {
        let error = b.last_error.clone().unwrap_or_default();
        return vec![(
            Mark::Warn,
            tf(
                l,
                "The last copy failed: {}. The data is safe on the server's disk, but not copied elsewhere.",
                &[&error.trim_end_matches('.')],
            ),
        )];
    }
    match b.last_at.as_deref().and_then(format::parse_time) {
        Some(at) => vec![(Mark::Ok, tf(l, "Last copy {}, to {}", &[&format::ago(l, at, now), &target]))],
        None => vec![(Mark::Off, tf(l, "Waiting for the first copy, to {}", &[&target]))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Backup, DbStatus, Encryption, LastConnection, StatusMemory};

    fn status() -> Status {
        Status {
            version: "0.4.7".into(),
            uptime_s: 3 * 86_400 + 60,
            memory: StatusMemory {
                total_bytes: Some(16 << 30),
                available_bytes: Some(6_657_199_309),
                process_bytes: Some(858_993_459),
            },
            backup: Backup {
                enabled: true,
                target: Some("s3://bucket/nuage".into()),
                last_at: Some("2026-11-15T21:00:00Z".into()),
                last_ok: true,
                last_error: None,
            },
            encryption: Encryption { enabled: true, scheme: Some("XTS-AES-256, AES-256-GCM".into()) },
            databases: vec![DbStatus {
                name: "shop".into(),
                last_connection: Some(LastConnection {
                    at: "2026-11-15T21:02:00Z".into(),
                    ..LastConnection::default()
                }),
                ..DbStatus::default()
            }],
        }
    }

    #[test]
    fn the_overview_says_it_in_words() {
        let now = format::parse_time("2026-11-15T21:04:00Z").unwrap();
        let s = status();
        assert_eq!(
            answers(Lang::Fr, &s),
            "en marche depuis 3 jours · version 0.4.7 · tout ce qu'il stocke est chiffré"
        );
        assert_eq!(
            applications(Lang::En, 3, Some(&s), &BTreeMap::new(), now),
            "3 applications · last connection 2 min ago (shop)"
        );
        assert_eq!(applications(Lang::Fr, 1, None, &BTreeMap::new(), now), "1 application");
        // A test from the editor is not the site's connection: the one seen before counts.
        let mut tested = s.clone();
        tested.databases[0].last_connection = Some(LastConnection {
            at: "2026-11-15T21:03:58Z".into(),
            client: "nuage-editor/0.4.7".into(),
            ..LastConnection::default()
        });
        assert_eq!(applications(Lang::En, 3, Some(&tested), &BTreeMap::new(), now), "3 applications");
        let seen = BTreeMap::from([("shop".to_owned(), s.databases[0].last_connection.clone().unwrap())]);
        assert_eq!(
            applications(Lang::En, 3, Some(&tested), &seen, now),
            "3 applications · last connection 2 min ago (shop)"
        );
        assert_eq!(
            memory(Lang::Fr, Some(&s), None).unwrap(),
            "Nuage DB occupe 0,8 Gio · ce serveur a 16,0 Gio, dont 6,2 Gio libres"
        );
        assert_eq!(
            backups(Lang::En, Some(&s), None, false, now),
            vec![(Mark::Ok, "Last copy 4 min ago, to s3://bucket/nuage".to_owned())]
        );
        let mut failing = s.clone();
        failing.backup.last_ok = false;
        failing.backup.last_error = Some("shop: the bucket refused the key.".into());
        let lines = backups(Lang::Es, Some(&failing), None, false, now);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, Mark::Warn);
        assert!(lines[0].1.contains("shop: the bucket refused the key") && !lines[0].1.contains(".."), "{lines:?}");
        let mut off = s.clone();
        off.backup.enabled = false;
        let lines = backups(Lang::Fr, Some(&off), None, true, now);
        assert!(lines.iter().all(|(m, _)| *m == Mark::Bad) && lines.len() == 2, "{lines:?}");
        let mut waiting = s;
        waiting.backup.last_at = None;
        assert_eq!(backups(Lang::En, Some(&waiting), None, false, now)[0].0, Mark::Off);
        // An older server: no word on the last copy, and what updating would show.
        let older = backups(Lang::En, None, None, false, now);
        assert!(older.len() == 1 && older[0].1.contains("update the server"), "{older:?}");
    }
}
