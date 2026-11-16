//! The applications: one database per website or program. Making one, its
//! tokens (made, shown once, revoked after asking), the websites whose pages
//! may call it, and its memory.

use eframe::egui::{self, RichText};

use super::{Confirm, Doing, OLDER_WEBSITES, Outcome, Session, TokenAsked, help, lan_for};
use crate::api::{self, Application, DbStatus, LastConnection};
use crate::format;
use crate::i18n::{Lang, t, tf};
use crate::theme::{self, Mark};

/// The list, and a new application.
pub(super) fn list_page(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    help(
        ui,
        t(
            l,
            "An application is a database for one website or program: its own data, encrypted, and its own tokens. Applications never see each other's data.",
        ),
    );
    ui.add_space(8.0);
    new_application(ui, session, l);
    ui.add_space(10.0);
    if let Some(name) = session.unsure.clone() {
        unsure(ui, session, &name, l);
        ui.add_space(10.0);
    }
    let Some(apps) = session.apps.clone() else {
        ui.spinner();
        return;
    };
    let now = format::now_ms();
    let mut open = None;
    for app in &apps {
        let db = session.status.as_ref().and_then(|s| s.database(&app.name));
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let name = egui::Button::new(RichText::new(&app.name).strong().size(17.0));
                if ui.add(name).on_hover_text(t(l, "Open its page")).clicked() {
                    open = Some(app.name.clone());
                }
                if app.first {
                    ui.label(RichText::new(t(l, "main")).color(theme::of(ui).accent));
                }
            });
            if app.first {
                help(ui, t(l, "created at installation; it cannot be deleted"));
            }
            if !app.description.is_empty() {
                ui.label(&app.description);
            }
            help(ui, tokens(l, app));
            if let Some(db) = db {
                let (m, line) = connection(l, db, session.sites.get(&app.name), now);
                theme::status(ui, m, line);
                help(
                    ui,
                    tf(
                        l,
                        "tables {} · rows {} · {}",
                        &[&db.tables, &format::count(l, db.rows), &format::bytes(l, db.bytes)],
                    ),
                );
                if db.status == "degraded" {
                    theme::status(ui, Mark::Warn, t(l, "Backup failing"));
                }
            }
        });
        ui.add_space(6.0);
    }
    if let [only] = apps.as_slice()
        && only.first
    {
        help(
            ui,
            tf(
                l,
                "For now there is only the main application, {}, created at installation. Make one per website, with New application above.",
                &[&only.name],
            ),
        );
    }
    help(ui, t(l, "An application missing here? The web console lists those the server could not open."));
    if let Some(name) = open {
        session.open_app = Some(name);
        session.notice = None;
        session.origin_error = None;
        session.origin_field.clear();
    }
}

/// "What is it for?", its name (suggested from it), and Create.
fn new_application(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    let ctx = ui.ctx().clone();
    let creating = session.action.as_ref().is_some_and(|(d, _)| *d == Doing::Create);
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::subheading(ui, t(l, "New application"));
        let label = ui.strong(t(l, "What is it for?"));
        ui.add(egui::TextEdit::singleline(&mut session.new_what).hint_text(t(l, "My shop")).desired_width(360.0))
            .labelled_by(label.id);
        if !session.name_typed {
            session.new_name = api::suggest_name(&session.new_what);
        }
        let label = ui.strong(t(l, "Name"));
        let field = ui
            .add(egui::TextEdit::singleline(&mut session.new_name).hint_text("ma-boutique").desired_width(360.0))
            .labelled_by(label.id);
        if field.changed() {
            session.name_typed = true;
        }
        let name = session.new_name.trim().to_owned();
        let valid = api::app_name_ok(&name);
        if valid {
            theme::status(ui, Mark::Ok, tf(l, "Its name will be: {}", &[&name]));
        }
        let rule =
            t(l, "1 to 63 lowercase letters, digits, - and _ (not main). It is also its database name for PostgreSQL.");
        if valid || name.is_empty() {
            help(ui, rule);
        } else {
            theme::status(ui, Mark::Warn, rule);
        }
        if let Some(e) = &session.new_error {
            theme::status(ui, Mark::Bad, e.clone());
        }
        let label = if creating { t(l, "Creating…") } else { t(l, "Create the application") };
        if ui.add_enabled(valid && !session.busy(), theme::primary(label, ui)).clicked() {
            let description = session.new_what.trim().to_owned();
            session.new_error = None;
            session.act(&ctx, Doing::Create, move |server| {
                let (app, connection) = server.create_application(&name, &description)?;
                let lan = lan_for(&connection);
                Ok(Outcome::Created(app, connection, lan))
            });
        }
    });
}

/// A creation whose answer was lost: was it made? Read the list again to know.
fn unsure(ui: &mut egui::Ui, session: &mut Session, name: &str, l: Lang) {
    let made = session.app(name).cloned();
    let mut check = false;
    let mut open = false;
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        match &made {
            Some(app) => {
                let at = app.tokens.first().map(|t| format::clock(t.created_at)).unwrap_or_default();
                theme::status(
                    ui,
                    Mark::Warn,
                    tf(
                        l,
                        "It was created, but its token never reached the editor: make it a new token, then revoke the lost one (made at {}).",
                        &[&at],
                    ),
                );
                open = ui.button(tf(l, "Open {}", &[&app.name])).clicked();
            }
            None => {
                theme::status(
                    ui,
                    Mark::Warn,
                    t(
                        l,
                        "The answer to the creation did not reach the editor: the application may have been created anyway.",
                    ),
                );
                check = ui.add_enabled(!session.loading(), egui::Button::new(t(l, "Check"))).clicked();
            }
        }
    });
    if check {
        session.reload(ui.ctx());
    }
    if open {
        session.open_app = Some(name.to_owned());
        session.unsure = None;
        // Its first token never reached the editor: its page marks it.
        session.token_asked = Some(TokenAsked { app: name.to_owned(), known: Vec::new(), lost: true });
    }
}

/// "2 tokens, 1 read-only", counting NUAGE_TOKEN as one.
fn tokens(l: Lang, app: &Application) -> String {
    let n = (app.tokens.len() + usize::from(app.env_token)) as u64;
    let read_only = app.tokens.iter().filter(|t| t.read_only).count() as u64;
    let all = match n {
        0 => return t(l, "No token: no program can connect yet.").to_owned(),
        _ if format::singular(l, n) => tf(l, "{} token", &[&n]),
        _ => tf(l, "{} tokens", &[&n]),
    };
    if read_only == 0 { all } else { tf(l, "{}, {} read-only", &[&all, &read_only]) }
}

/// Whether its site connected: "Connected 2 min ago (https://monsite.fr)".
/// The server keeps the last client only, so a test from the editor hides
/// the connection before it: `earlier` is the last one the editor saw that
/// was not its own.
fn connection(l: Lang, db: &DbStatus, earlier: Option<&LastConnection>, now: i64) -> (Mark, String) {
    let ago = |c: &LastConnection| format::parse_time(&c.at).map(|at| format::ago(l, at, now)).unwrap_or_default();
    let site = match &db.last_connection {
        None => return (Mark::Off, t(l, "Waiting for a first connection").to_owned()),
        Some(c) if c.by_the_editor() => match earlier {
            Some(earlier) => earlier,
            // Nothing is known of the site: the test may have hidden its connection.
            None => return (Mark::Ok, tf(l, "Tested from the editor {}", &[&ago(c)])),
        },
        Some(c) => c,
    };
    let who = if site.client.is_empty() { site.via.clone() } else { site.client.clone() };
    (Mark::On, tf(l, "Connected {} ({})", &[&ago(site), &who]))
}

/// One application: its connection, its tokens, its websites, its memory.
pub(super) fn app_page(ui: &mut egui::Ui, session: &mut Session, name: &str, l: Lang) {
    let ctx = ui.ctx().clone();
    ui.horizontal_wrapped(|ui| {
        if ui.link(t(l, "Applications")).clicked() {
            session.open_app = None;
        }
        ui.label("›");
        ui.label(RichText::new(name).strong());
    });
    if session.open_app.is_none() {
        return;
    }
    let Some(app) = session.app(name).cloned() else {
        theme::status(
            ui,
            Mark::Warn,
            tf(l, "There is no application {} any more (deleted from the console?).", &[&name]),
        );
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.heading(&app.name);
        if app.first {
            ui.label(RichText::new(t(l, "main")).color(theme::of(ui).accent));
        }
    });
    if !app.description.is_empty() {
        ui.label(&app.description);
    }
    ui.add_space(8.0);
    let db = session.status.as_ref().and_then(|s| s.database(name)).cloned();
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::subheading(ui, t(l, "Connection"));
        if let Some(db) = &db {
            let (m, line) = connection(l, db, session.sites.get(name), format::now_ms());
            theme::status(ui, m, line);
        }
        let lines = format!(
            "NUAGE_URL={}\nNUAGE_TOKEN=<{}>",
            session.server.public_address(),
            t(l, "a token of this application")
        );
        code(ui, &lines);
        help(ui, t(l, "A token is shown only when it is made: make a new one to connect another site."));
        help(
            ui,
            t(
                l,
                "The token also works as the PostgreSQL password of this application (its database name is its name).",
            ),
        );
    });
    ui.add_space(8.0);
    tokens_card(ui, session, &app, l);
    ui.add_space(8.0);
    websites_card(ui, session, &app, l);
    ui.add_space(8.0);
    memory_card(ui, session, &app, l);
    ui.add_space(8.0);
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::subheading(ui, t(l, "Delete"));
        if app.first {
            ui.label(t(l, "The main application cannot be deleted."));
        } else {
            ui.label(t(
                l,
                "Deleting an application erases its database and its backups. It is done in the web console, where you type its name again to confirm. The console asks for its password (NUAGE_ADMIN_PASSWORD, set with the server).",
            ));
            if ui.button(t(l, "Open the console on this application")).clicked() {
                ctx.open_url(egui::OpenUrl::new_tab(console_of(session, &app.name)));
            }
        }
    });
}

/// The console's page of the application `name`.
fn console_of(session: &Session, name: &str) -> String {
    let segment: String = name
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => char::from(b).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("{}/apps/{segment}/settings", session.server.console_url())
}

fn tokens_card(ui: &mut egui::Ui, session: &mut Session, app: &Application, l: Lang) {
    let ctx = ui.ctx().clone();
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::subheading(ui, t(l, "Tokens"));
        if app.env_token {
            help(
                ui,
                t(
                    l,
                    "NUAGE_TOKEN (set on the server): read and write; it changes in the server's settings, not here.",
                ),
            );
        }
        if app.tokens.is_empty() && !app.env_token {
            help(ui, t(l, "No token: no program can connect yet."));
        }
        // A new token whose answer was lost: once the list is read again, the one made is marked.
        let lost = session.token_asked.as_ref().filter(|a| a.lost && a.app == app.name).map(|a| a.known.clone());
        if let Some(known) = &lost
            && !session.loading()
        {
            if app.tokens.iter().any(|t| !known.contains(&t.id)) {
                theme::status(
                    ui,
                    Mark::Warn,
                    t(
                        l,
                        "A token was made, but it never reached the editor: nobody has it. Revoke the one marked never shown; make a new one if you need it.",
                    ),
                );
            } else {
                theme::status(ui, Mark::Off, t(l, "No new token was made: make it again if you need it."));
            }
        }
        for token in &app.tokens {
            ui.horizontal_wrapped(|ui| {
                ui.monospace(format!("nuage_{}…", token.id));
                let kind = if token.read_only { t(l, "read only") } else { t(l, "read and write") };
                ui.label(tf(l, "{} · made {}", &[&kind, &format::date(l, token.created_at)]));
                if lost.as_ref().is_some_and(|known| !known.contains(&token.id)) {
                    ui.label(RichText::new(t(l, "never shown")).color(Mark::Warn.color(theme::of(ui))));
                }
                // Each says which token it revokes (a screen reader reads its name alone).
                let revoke = egui::Button::new(t(l, "Revoke")).small();
                let revoke = ui
                    .add_enabled(!session.busy(), revoke)
                    .accessible_name(tf(l, "Revoke nuage_{}…", &[&token.id]));
                if revoke.clicked() {
                    session.confirm = Some(Confirm { app: app.name.clone(), id: token.id.clone(), focused: false });
                }
            });
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for (read_only, label) in
                [(false, t(l, "New token (read and write)")), (true, t(l, "New read-only token, for a web page"))]
            {
                if ui.add_enabled(!session.busy(), egui::Button::new(label)).clicked() {
                    let name = app.name.clone();
                    let known = app.tokens.iter().map(|t| t.id.clone()).collect();
                    session.token_asked = Some(TokenAsked { app: name.clone(), known, lost: false });
                    session.act(&ctx, Doing::Token, move |server| {
                        let connection = server.new_token(&name, read_only)?;
                        let lan = lan_for(&connection);
                        Ok(Outcome::Token { app: name, read_only, connection, lan })
                    });
                }
            }
        });
        help(
            ui,
            t(
                l,
                "A read-and-write token stays on a server. A web page shows its code to every visitor: give it a read-only token, which can only read.",
            ),
        );
    });
}

fn websites_card(ui: &mut egui::Ui, session: &mut Session, app: &Application, l: Lang) {
    let ctx = ui.ctx().clone();
    let can = session.server.caps().origins;
    let mut send: Option<Vec<String>> = None;
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::subheading(ui, t(l, "Websites allowed"));
        help(
            ui,
            t(
                l,
                "Pages of these websites may call this database from their visitors' browsers. Write the start of the address, without a path: https://monsite.fr; to try on this computer, http://localhost:3000.",
            ),
        );
        if app.origins.is_empty() {
            help(ui, t(l, "None yet: browsers refuse the calls of every page."));
        }
        for origin in &app.origins {
            ui.horizontal_wrapped(|ui| {
                if origin == "*" {
                    ui.monospace("*");
                    ui.label(t(l, "any website"));
                } else {
                    ui.monospace(origin);
                }
                let remove = egui::Button::new(t(l, "Remove")).small();
                if can
                    && ui
                        .add_enabled(!session.busy(), remove)
                        .accessible_name(tf(l, "Remove {}", &[origin]))
                        .on_hover_text(origin)
                        .clicked()
                {
                    send = Some(app.origins.iter().filter(|o| *o != origin).cloned().collect());
                }
            });
        }
        if app.origins.iter().any(|o| o.starts_with("https://")) && session.cleartext_public() {
            theme::status(
                ui,
                Mark::Warn,
                t(
                    l,
                    "Your site is in https and Nuage DB in http: browsers will block the calls. Nuage DB needs an https address.",
                ),
            );
        }
        if can {
            let label = ui.strong(t(l, "Website to allow"));
            let mut add = false;
            ui.horizontal_wrapped(|ui| {
                let field = ui
                    .add(
                        egui::TextEdit::singleline(&mut session.origin_field)
                            .hint_text("https://monsite.fr")
                            .desired_width(300.0),
                    )
                    .labelled_by(label.id);
                if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    add = true;
                }
                add |= ui.add_enabled(!session.busy(), egui::Button::new(t(l, "Add"))).clicked();
            });
            if add {
                match api::origin(&session.origin_field) {
                    None => {
                        session.origin_error = Some(t(
                            l,
                            "That is not the start of a website's address: write its scheme and its name, without a path, such as https://monsite.fr.",
                        )
                        .to_owned());
                    }
                    Some(o) if app.origins.contains(&o) => {
                        session.origin_error = Some(tf(l, "{} is already allowed.", &[&o]));
                    }
                    Some(o) => {
                        let mut list = app.origins.clone();
                        list.push(o);
                        send = Some(list);
                    }
                }
            }
        }
        if let Some(e) = &session.origin_error {
            theme::status(ui, Mark::Bad, e.clone());
        }
        if !can {
            theme::status(ui, Mark::Warn, t(l, OLDER_WEBSITES));
        }
        if (!can || session.origin_error.as_deref() == Some(t(l, OLDER_WEBSITES)))
            && ui.button(t(l, "Open it on this application")).clicked()
        {
            ctx.open_url(egui::OpenUrl::new_tab(console_of(session, &app.name)));
        }
    });
    if let Some(list) = send {
        let name = app.name.clone();
        session.origin_error = None;
        session.act(&ctx, Doing::Origins, move |server| server.set_origins(&name, &list).map(Outcome::Origins));
    }
}

fn memory_card(ui: &mut egui::Ui, session: &mut Session, app: &Application, l: Lang) {
    let ctx = ui.ctx().clone();
    let read_only = session.settings.as_ref().is_some_and(|s| s.read_only);
    let held = session.settings.as_ref().and_then(|s| s.applications.iter().find(|a| a.name == app.name)).cloned();
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        theme::subheading(ui, t(l, "Memory of this application"));
        if !session.server.caps().settings {
            help(ui, t(l, super::settings::NO_SETTINGS));
            return;
        }
        help(
            ui,
            t(
                l,
                "auto: shared with the others, by use. An amount: set apart for it (512 MiB, 2 GiB; a number alone is in MiB).",
            ),
        );
        if let Some(held) = &held {
            help(
                ui,
                tf(
                    l,
                    "Its cache holds {} of {} now.",
                    &[&format::bytes(l, held.cache_bytes), &format::bytes(l, held.cache_capacity_bytes)],
                ),
            );
        }
        let (cache, kv) = session
            .app_fields
            .entry(app.name.clone())
            .or_insert_with(|| (format::amount(l, app.memory.cache_mb), format::amount(l, app.memory.kv_mb)));
        egui::Grid::new("app-memory").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            let label = ui.label(t(l, "Page cache"));
            ui.add_enabled(!read_only, egui::TextEdit::singleline(cache).desired_width(140.0)).labelled_by(label.id);
            ui.end_row();
            let label = ui.label(t(l, "Key-value store"));
            ui.add_enabled(!read_only, egui::TextEdit::singleline(kv).desired_width(140.0)).labelled_by(label.id);
            ui.end_row();
        });
        let (cache, kv) = (cache.clone(), kv.clone());
        if ui.add_enabled(!session.busy() && !read_only, egui::Button::new(t(l, "Save its memory"))).clicked() {
            let name = app.name.clone();
            session.act(&ctx, Doing::Memory, move |server| {
                server.set_memory(&name, Some(&cache), Some(&kv)).map(Outcome::Memory)
            });
        }
    });
}

/// Lines to copy by hand, in monospace (selectable, never an editable field).
pub(super) fn code(ui: &mut egui::Ui, text: &str) {
    code_block(ui, text, true);
}

/// Lines that hold a secret: not selectable, so a selection and Ctrl+C
/// never copy it the ordinary way (the clipboard history would keep it);
/// its Copy buttons ask the history to skip it.
pub(super) fn secret_code(ui: &mut egui::Ui, text: &str) {
    code_block(ui, text, false);
}

fn code_block(ui: &mut egui::Ui, text: &str, selectable: bool) {
    let p = theme::of(ui);
    egui::Frame::new().fill(p.code).stroke(egui::Stroke::new(1.0, p.line)).corner_radius(8).inner_margin(10).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(selectable));
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Token;

    #[test]
    fn rows_say_it_in_words() {
        let now = format::parse_time("2026-11-15T21:04:05Z").unwrap();
        let mut app = Application { name: "shop".into(), env_token: true, ..Application::default() };
        app.tokens = vec![Token { read_only: true, ..Token::default() }];
        assert_eq!(tokens(Lang::Fr, &app), "2 jetons, dont 1 en lecture seule");
        assert_eq!(tokens(Lang::En, &Application::default()), "No token: no program can connect yet.");
        app.env_token = false;
        assert_eq!(tokens(Lang::Es, &app), "1 token, 1 de solo lectura");
        let mut db = DbStatus { name: "shop".into(), ..DbStatus::default() };
        assert_eq!(connection(Lang::Fr, &db, None, now), (Mark::Off, "En attente d'une première connexion".to_owned()));
        let site = LastConnection {
            at: "2026-11-15T21:02:05Z".into(),
            client: "https://monsite.fr".into(),
            ..LastConnection::default()
        };
        db.last_connection = Some(site.clone());
        assert_eq!(
            connection(Lang::En, &db, None, now),
            (Mark::On, "Connected 2 min ago (https://monsite.fr)".to_owned())
        );
        // The editor's test takes the site's place on the server: it never says the site did not connect.
        db.last_connection = Some(LastConnection {
            at: "2026-11-15T21:04:00Z".into(),
            client: "nuage-editor/0.4.7".into(),
            ..LastConnection::default()
        });
        assert_eq!(connection(Lang::Fr, &db, None, now), (Mark::Ok, "Testée depuis l'éditeur il y a 5 s".to_owned()));
        assert_eq!(
            connection(Lang::En, &db, Some(&site), now),
            (Mark::On, "Connected 2 min ago (https://monsite.fr)".to_owned())
        );
    }
}
