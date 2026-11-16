//! The windows above the page: a new token, shown once until its owner
//! closes it (Escape and clicks outside do not), and a revocation to confirm.

use eframe::egui;
use zeroize::Zeroizing;

use super::{COPIED, Doing, Lan, Made, Outcome, Reveal, Section, Session, help};
use crate::address;
use crate::api::{ApiError, Ping};
use crate::clip::{Copied, CopySink};
use crate::i18n::{Lang, t, tf};
use crate::jobs::{Job, take};
use crate::secret::Secret;
use crate::theme::{self, Mark};

/// What a masked token shows instead of its characters.
pub const MASK: &str = "••••••••••••";

/// The card of a new token.
pub(super) fn reveal(ctx: &egui::Context, session: &mut Session, l: Lang, clipboard: &mut dyn CopySink) {
    // The test's answer: the application's page then says the editor tested it.
    let tested = session.reveal.as_mut().and_then(|r| take(&mut r.test).map(|answer| r.tested = Some(answer)));
    if tested.is_some() {
        session.reload(ctx);
    }
    let websites = session.reveal.as_ref().map(|r| session.websites_of(&r.app)).unwrap_or_default();
    let base = session.server.address().to_owned();
    let server = session.server.clone();
    let calls = session.calls.clone();
    let Some(reveal) = session.reveal.as_mut() else { return };
    let mut close = false;
    let mut go_websites = false;
    let max_height = (ctx.content_rect().height() - 60.0).max(200.0);
    egui::Modal::new(egui::Id::unique("reveal")).show(ctx, |ui| {
        ui.set_max_width(620.0);
        // Taller than the window, it scrolls; its height is set, not left to the first frame's
        // guess (an anchored window sized too small would hide its last buttons).
        egui::ScrollArea::vertical().max_height(max_height).min_scrolled_height(max_height).show(ui, |ui| {
            ui.heading(t(l, "Copy this token now"));
            ui.label(match (reveal.made, reveal.read_only) {
                (Made::Created, _) => tf(l, "Application {} created", &[&reveal.app]),
                (Made::Token, false) => tf(l, "New token for {}", &[&reveal.app]),
                (Made::Token, true) => tf(l, "New read-only token for {}", &[&reveal.app]),
            });
            help(
                ui,
                if reveal.read_only {
                    t(l, "Read only: it may sit in a web page; nobody can change or erase your data with it.")
                } else {
                    t(l, "Read and write: for a server or a program, never in a web page.")
                },
            );
            theme::status(
                ui,
                Mark::Warn,
                t(
                    l,
                    "It will never be shown again, here or in the console: the server keeps only a fingerprint of it. If you lose it, make another one.",
                ),
            );
            ui.add_space(6.0);
            ui.label(t(l, "For your website or program (its .env file, or its host's variables):"));
            let token = Zeroizing::new(if reveal.shown {
                reveal.connection.token.expose().to_owned()
            } else {
                format!("nuage_{MASK}")
            });
            let lines = Zeroizing::new(format!("NUAGE_URL={}\nNUAGE_TOKEN={}", reveal.connection.url, *token));
            super::apps::secret_code(ui, &lines);
            ui.horizontal_wrapped(|ui| {
                let show = if reveal.shown { t(l, "Hide the token") } else { t(l, "Show the token") };
                if ui.button(show).clicked() {
                    reveal.shown = !reveal.shown;
                }
                if ui.add(theme::primary(t(l, "Copy both lines"), ui)).clicked() {
                    reveal.copied = Some(clipboard.copy_secret(ui.ctx(), &reveal.connection.env_lines()));
                    reveal.cleared = false;
                }
                if ui.button(t(l, "Copy the token only")).clicked() {
                    reveal.copied = Some(clipboard.copy_secret(ui.ctx(), reveal.connection.token.expose()));
                    reveal.cleared = false;
                }
                let testing = reveal.test.is_some();
                if ui.add_enabled(!testing, egui::Button::new(t(l, "Test the token"))).clicked() {
                    let token = Secret::new(reveal.connection.token.expose().to_owned());
                    let server = server.clone();
                    reveal.tested = None;
                    reveal.test = Some(Job::start(ui.ctx(), &calls, move || server.ping_with(&token)));
                }
                if testing {
                    ui.spinner();
                }
            });
            copied(ui, reveal, clipboard, l);
            if let Some(answer) = &reveal.tested {
                let (m, text) = test_answer(l, answer, &base);
                theme::status(ui, m, text);
            }
            if reveal.connection.is_this_computer_only() {
                let note = match (&reveal.lan, address::normalize(&reveal.connection.url)) {
                    (Lan::HereOnly(bind), _) => tf(
                        l,
                        "This address works for a program on this computer: the server answers this computer only (NUAGE_BIND={}). For another device of your network, set NUAGE_BIND=0.0.0.0 in the server's settings, then restart it. A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
                        &[bind],
                    ),
                    (Lan::At(ip), Ok(url)) => tf(
                        l,
                        "This address works for a program on this computer. On another device of your network, use http://{}:{} instead (if this computer's firewall lets it in). A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
                        &[ip, &url.port_or_default()],
                    ),
                    _ => t(
                        l,
                        "This address works for a program on this computer. A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
                    )
                    .to_owned(),
                };
                theme::status(ui, Mark::Warn, note);
            }
            if reveal.read_only {
                ui.add_space(6.0);
                theme::subheading(ui, t(l, "In a web page"));
                if websites.is_empty() {
                    theme::status(
                        ui,
                        Mark::Warn,
                        t(
                            l,
                            "No website may call this application yet: browsers will refuse calls from a page until you add its address under Websites allowed.",
                        ),
                    );
                    go_websites = ui.button(t(l, "Go to Websites allowed")).clicked();
                } else {
                    help(ui, tf(l, "Websites allowed: {}.", &[&websites.join(", ")]));
                }
                ui.label(t(l, "Paste this in the page's HTML, where the page reads its data:"));
                let page = Zeroizing::new(page_snippet(l, &reveal.connection.url, &token));
                super::apps::secret_code(ui, &page);
                if ui.button(t(l, "Copy for the web page")).clicked() {
                    let snippet =
                        Zeroizing::new(page_snippet(l, &reveal.connection.url, reveal.connection.token.expose()));
                    reveal.copied = Some(clipboard.copy_secret(ui.ctx(), &snippet));
                    reveal.cleared = false;
                }
            }
            ui.add_space(6.0);
            help(
                ui,
                t(
                    l,
                    "Then restart your website or program so it reads these lines. Its application page shows its first connection.",
                ),
            );
            ui.add_space(6.0);
            if reveal.closing {
                theme::status(ui, Mark::Warn, t(l, "Close without copying the token? It cannot be shown again."));
                ui.horizontal(|ui| {
                    if ui.add(theme::primary(t(l, "Back"), ui)).clicked() {
                        reveal.closing = false;
                    }
                    if ui.button(t(l, "Close anyway")).clicked() {
                        close = true;
                    }
                });
            } else if ui.add(theme::primary(t(l, "I have copied it"), ui)).clicked() {
                if reveal.taken() {
                    close = true;
                } else {
                    reveal.closing = true;
                }
            }
        });
    });
    if go_websites {
        session.section = Section::Applications;
        session.open_app = Some(reveal.app.clone());
        if reveal.taken() {
            close = true;
        } else {
            reveal.closing = true;
        }
    }
    if close {
        // The token is wiped (Secret) when the card goes.
        session.reveal = None;
    }
}

/// What the page holds: the address, the read-only token, a call. A module
/// script (`type="module"`), where `await` may stand outside a function.
fn page_snippet(l: Lang, url: &str, token: &str) -> String {
    let table = t(l, "articles: the name of one of your tables");
    format!(
        r#"<script type="module">
const NUAGE_URL = "{url}";
const NUAGE_TOKEN = "{token}";
// {table}
const res = await fetch(NUAGE_URL + "/v1/sql", {{
  method: "POST",
  headers: {{ "Authorization": "Bearer " + NUAGE_TOKEN, "Content-Type": "application/json" }},
  body: JSON.stringify({{ sql: "SELECT * FROM articles LIMIT 20" }}),
}});
const {{ rows, error }} = await res.json();
console.log(rows ?? error);
</script>"#
    )
}

/// How the last copy went, and the way to empty the clipboard.
fn copied(ui: &mut egui::Ui, reveal: &mut Reveal, clipboard: &mut dyn CopySink, l: Lang) {
    match &reveal.copied {
        Some(_) if reveal.cleared => help(ui, t(l, "The clipboard is empty again.")),
        Some(Copied::Private) => {
            theme::status(ui, Mark::Ok, t(l, COPIED));
            if ui.button(t(l, "Clear the clipboard")).clicked() {
                clipboard.clear(ui.ctx());
                reveal.cleared = true;
            }
        }
        Some(Copied::Plain(why)) => {
            theme::status(
                ui,
                Mark::Warn,
                tf(l, "Copied, but the clipboard history could not be asked to skip it ({}): it may keep it.", &[why]),
            );
            if ui.button(t(l, "Clear the clipboard")).clicked() {
                clipboard.clear(ui.ctx());
                reveal.cleared = true;
            }
        }
        None => {}
    }
}

/// What the test of a new token says.
pub(super) fn test_answer(l: Lang, answer: &Result<Ping, ApiError>, base: &str) -> (Mark, String) {
    match answer {
        Ok(ping) => {
            let access = if ping.read_only() { t(l, "read only") } else { t(l, "read and write") };
            (
                Mark::Ok,
                tf(
                    l,
                    "It works: it opens {}, {} ({} ms), tested from this computer through {}.",
                    &[&ping.application, &access, &ping.millis, &base],
                ),
            )
        }
        Err(ApiError::TokenRefused) => {
            (Mark::Bad, t(l, "The server does not recognise this token: make another one.").to_owned())
        }
        Err(e) => (
            Mark::Warn,
            format!(
                "{} ({})",
                t(l, "The test could not reach the server; the token itself is fine. Test again in a moment."),
                e.explain(l)
            ),
        ),
    }
}

/// "Revoke this token?": Keep it has the focus, and Escape keeps it.
pub(super) fn revoke(ctx: &egui::Context, session: &mut Session, l: Lang) {
    let Some(confirm) = session.confirm.as_mut() else { return };
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::unique("revoke")).show(ctx, |ui| {
        ui.set_max_width(480.0);
        theme::subheading(ui, t(l, "Revoke this token?"));
        ui.label(tf(
            l,
            "nuage_{}… of {} will open nothing any more: what uses it (a site, a script) loses access at once. It cannot be undone; make a new token instead.",
            &[&confirm.id, &confirm.app],
        ));
        ui.horizontal(|ui| {
            if ui.add(theme::danger(t(l, "Revoke"), ui)).clicked() {
                answer = Some(true);
            }
            let keep = ui.button(t(l, "Keep it"));
            if !confirm.focused {
                keep.request_focus();
                confirm.focused = true;
            }
            if keep.clicked() {
                answer = Some(false);
            }
        });
    });
    if modal.should_close() {
        answer = Some(false);
    }
    match answer {
        Some(true) => {
            if let Some(super::Confirm { app, id, .. }) = session.confirm.take() {
                session.act(ctx, Doing::Revoke, move |server| {
                    server.revoke_token(&app, &id).map(|()| Outcome::Revoked { app, id })
                });
            }
        }
        Some(false) => session.confirm = None,
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_says_what_the_token_opens() {
        let ping = Ping { application: "shop".into(), access: "read-write".into(), millis: 12, ..Ping::default() };
        let (m, text) = test_answer(Lang::Fr, &Ok(ping), "https://n.example.com");
        assert_eq!(m, Mark::Ok);
        assert_eq!(
            text,
            "Il fonctionne : il ouvre shop, en lecture et écriture (12 ms), testé depuis cet ordinateur via https://n.example.com."
        );
        assert_eq!(test_answer(Lang::En, &Err(ApiError::TokenRefused), "x").0, Mark::Bad);
        let unreachable = test_answer(Lang::Es, &Err(ApiError::Busy), "x");
        assert_eq!(unreachable.0, Mark::Warn);
        let snippet = page_snippet(Lang::Fr, "https://n.example.com", "nuage_abc");
        assert!(snippet.contains("\"https://n.example.com\"") && snippet.contains("\"nuage_abc\""), "{snippet}");
        // Its `await` stands outside a function: only a module script allows that.
        assert!(snippet.starts_with("<script type=\"module\">\n") && snippet.ends_with("</script>"), "{snippet}");
        assert!(snippet.contains("// articles : le nom d'une de vos tables"), "{snippet}");
    }
}
