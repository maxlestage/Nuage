//! Tools (what `nuage check` and `nuage genkey` do), the server of this
//! computer (whether it answers, where its files are, starting it when it is
//! stopped), and About (the preferences, and what the editor keeps).

use std::time::Duration;

use eframe::egui;
use zeroize::Zeroizing;

use super::{Editor, Generated, LocalServer, Notice, Starting, help, notice};
use crate::api::{ApiError, Server};
use crate::clip::{Copied, CopySink};
use crate::i18n::{Lang, t, tf};
use crate::jobs::{Job, take};
use crate::local::{self, Kind};
use crate::secret::Secret;
use crate::theme::{self, Mark};

/// 32 random bytes as 64 lowercase hex digits, as `nuage genkey` makes them.
pub fn genkey() -> Result<Secret, String> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut *bytes).map_err(|e| e.to_string())?;
    let mut hex = Zeroizing::new(String::with_capacity(64));
    for b in bytes.iter() {
        for digit in [b >> 4, b & 0x0f] {
            hex.push(char::from_digit(u32::from(digit), 16).unwrap_or('0'));
        }
    }
    Ok(Secret::new(std::mem::take(&mut *hex)))
}

/// "Generate a token": a masked card, with Show and Copy.
pub(super) fn generator(ui: &mut egui::Ui, generated: &mut Option<Generated>, clipboard: &mut dyn CopySink, l: Lang) {
    let mut again = false;
    match generated {
        None => again = ui.button(t(l, "Generate a token")).clicked(),
        Some(g) => {
            let shown = if g.shown { g.token.expose().to_owned() } else { super::dialogs::MASK.repeat(4) };
            super::apps::secret_code(ui, &Zeroizing::new(shown));
            ui.horizontal_wrapped(|ui| {
                let show = if g.shown { t(l, "Hide the token") } else { t(l, "Show the token") };
                if ui.button(show).clicked() {
                    g.shown = !g.shown;
                }
                if ui.button(t(l, "Copy the token")).clicked() {
                    g.copied = Some(clipboard.copy_secret(ui.ctx(), g.token.expose()));
                }
                again = ui.button(t(l, "Generate another")).clicked();
            });
            match &g.copied {
                Some(Copied::Private) => theme::status(ui, Mark::Ok, t(l, super::COPIED)),
                Some(Copied::Plain(why)) => theme::status(
                    ui,
                    Mark::Warn,
                    tf(
                        l,
                        "Copied, but the clipboard history could not be asked to skip it ({}): it may keep it.",
                        &[why],
                    ),
                ),
                None => {}
            }
        }
    }
    if again {
        *generated = match genkey() {
            Ok(token) => Some(Generated { token, shown: false, copied: None }),
            Err(why) => {
                theme::status(ui, Mark::Bad, tf(l, "This system gave no random numbers: {}", &[&why]));
                None
            }
        };
    }
}

impl Editor {
    /// The answers of the tools' jobs, and of a start of this computer's server.
    pub(super) fn poll_tools(&mut self, ctx: &egui::Context, l: Lang) {
        if let Some(answer) = take(&mut self.token_test.job) {
            self.token_test.answer = Some(answer);
        }
        if let Some(starting) = &self.starting
            && let Some(answer) = starting.job.poll()
        {
            self.starting = None;
            let log = log_place(l);
            self.start_note = Some(match answer {
                Ok(Ok(true)) => Notice::ok(t(l, "Nuage DB answers.")),
                Ok(Ok(false)) => Notice::bad(tf(
                    l,
                    "It did not start within 60 s. Its log says why: {}. Restarting the computer starts it too.",
                    &[&log],
                )),
                Ok(Err(e)) => Notice::bad(format!("{} {}", super::local_error(l, e), tf(l, "Its log: {}.", &[&log]))),
                Err(e) => Notice::bad(e.explain(l)),
            });
            // What answers now, whatever page is open (the start's card, the sections).
            self.here = None;
            self.look_here(ctx);
        }
    }

    pub(super) fn tools_page(&mut self, ui: &mut egui::Ui, l: Lang) {
        ui.add_space(8.0);
        self.back_link(ui, l);
        theme::page_title(ui, t(l, "Tools"), None);
        let ctx = ui.ctx().clone();
        if self.token_test.address.is_empty() {
            self.token_test.address = self
                .session
                .as_ref()
                .map(|s| s.server.address().to_owned())
                .unwrap_or_else(|| self.connect.address.clone());
        }
        let mut test = false;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Test an application's connection"));
            help(ui, t(l, "What a website or a program does: it opens Nuage DB with an application's token."));
            let label = theme::strong_label(ui, t(l, "Server address"));
            ui.add(egui::TextEdit::singleline(&mut self.token_test.address).desired_width(380.0)).labelled_by(label.id);
            // As for the management token: plain http to the Internet waits for a consent, each time.
            let cleartext = crate::address::normalize(&self.token_test.address)
                .ok()
                .filter(|a| a.is_cleartext_public())
                .map(|a| a.base());
            if let Some(base) = &cleartext {
                theme::status(
                    ui,
                    Mark::Bad,
                    t(l, "Not encrypted: the token would cross the Internet in clear. Use the server's https:// address."),
                );
                let mut anyway = self.token_test.cleartext_ok.as_deref() == Some(base.as_str());
                if ui.checkbox(&mut anyway, t(l, "Connect anyway")).changed() {
                    self.token_test.cleartext_ok = anyway.then(|| base.clone());
                }
            }
            let consented = cleartext.is_none() || self.token_test.cleartext_ok == cleartext;
            let label = theme::strong_label(ui, t(l, "Application token"));
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut *self.token_test.token)
                        .password(true)
                        .hint_text("nuage_…")
                        .desired_width(380.0),
                )
                .labelled_by(label.id);
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                test = true;
            }
            let running = self.token_test.job.is_some();
            ui.horizontal(|ui| {
                test |= ui.add_enabled(!running && consented, theme::primary(t(l, "Test"), ui)).clicked();
                if running {
                    ui.spinner();
                }
            });
            test &= consented && !running;
            if let Some(answer) = &self.token_test.answer {
                let (m, text) = match answer {
                    Ok(ping) => (
                        Mark::Ok,
                        tf(
                            l,
                            "Connected to Nuage DB {}: application {}, {}, branch {}, {} table(s) ({} ms).",
                            &[
                                &ping.version,
                                &ping.application,
                                &if ping.read_only() { t(l, "read only") } else { t(l, "read and write") },
                                &ping.branch,
                                &ping.tables,
                                &ping.millis,
                            ],
                        ),
                    ),
                    Err(ApiError::TokenRefused) => {
                        (Mark::Bad, t(l, "The server answered, but refused this token.").to_owned())
                    }
                    Err(ApiError::Unsupported | ApiError::TooOld) => (
                        Mark::Bad,
                        t(l, "Not a Nuage DB server of version 0.4 or later, or not this address.").to_owned(),
                    ),
                    Err(e) => (Mark::Bad, e.explain(l)),
                };
                theme::status(ui, m, text);
            }
        });
        if test {
            // The field is emptied (and wiped) as the test starts: nothing remembers the token.
            let token = Secret::new(std::mem::take(&mut *self.token_test.token).trim().to_owned());
            let address = self.token_test.address.clone();
            let (lang, transport) = (l, self.transport.clone());
            self.token_test.answer = None;
            self.token_test.cleartext_ok = None;
            self.token_test.job = Some(Job::start(&ctx, &self.calls, move || {
                if token.is_empty() {
                    return Err(ApiError::TokenRefused);
                }
                // The health first, without the token: a wrong address, a website or a captive
                // portal never gets it.
                let server = Server::new(&address, token, lang, &transport)?;
                server.health()?;
                server.ping()
            }));
        }
        ui.add_space(10.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Generate a token"));
            ui.label(t(
                l,
                "For NUAGE_ADMIN_TOKEN (Heroku Config Vars, Docker .env), or the NUAGE_ENCRYPTION_KEY of a new server. Never replace the NUAGE_ENCRYPTION_KEY of a server that already has data: its data and backups could no longer be read.",
            ));
            generator(ui, &mut self.generated, self.clipboard.as_mut(), l);
        });
    }

    /// "Back" to the start, for the pages shown before connecting.
    fn back_link(&mut self, ui: &mut egui::Ui, l: Lang) {
        if self.session.is_none() && ui.link(t(l, "Back to the start")).clicked() {
            self.aside = None;
        }
    }

    pub(super) fn about_page(&mut self, ui: &mut egui::Ui, l: Lang) {
        ui.add_space(8.0);
        self.back_link(ui, l);
        theme::page_title(ui, t(l, "About"), None);
        let ctx = ui.ctx().clone();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Preferences"));
            ui.horizontal_wrapped(|ui| {
                self.lang_combo(ui, l, false);
                self.theme_combo(ui, l);
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(t(l, "Text size"));
                for size in [100u16, 125, 150] {
                    ui.selectable_value(&mut self.prefs.text_size, size, format!("{size} %"));
                }
            });
            help(ui, t(l, "Ctrl + and Ctrl − change it too, until the editor closes."));
        });
        ui.add_space(10.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "What the editor keeps"));
            ui.label(tf(
                l,
                "Nuage DB Editor {}. It keeps no data of its own besides your preferences, and sends nothing to anyone but your servers.",
                &[&env!("CARGO_PKG_VERSION")],
            ));
            match &self.storage_dir {
                Some(dir) => help(
                    ui,
                    tf(l, "Your preferences are kept in {} (never a token).", &[&dir.join("app.ron").display()]),
                ),
                None => help(ui, t(l, "Your preferences are kept until the editor closes (never a token).")),
            }
            let kept = self.prefs.servers.iter().filter(|s| s.token_kept).count();
            help(ui, tf(l, "Kept tokens: {}, in {}.", &[&kept, &self.vault.store().place(l)]));
            let busy = self.forget_all_job.is_some();
            if ui.add_enabled(!busy, theme::danger(t(l, "Forget everything on this computer"), ui)).clicked() {
                self.forget_all_ask = true;
            }
            if let Some(n) = &self.start_note {
                notice(ui, n);
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                ui.add_space(6.0);
                if ui.button(t(l, "Add the editor to the applications menu")).clicked() {
                    self.start_note = Some(match add_to_menu() {
                        Ok(file) => Notice::ok(tf(
                            l,
                            "Added: {}. If you move the editor, press it again.",
                            &[&file.display()],
                        )),
                        Err(why) => Notice::bad(why),
                    });
                }
            }
        });
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            if ui.button(t(l, "Open the guide")).clicked() {
                ctx.open_url(egui::OpenUrl::new_tab("https://github.com/maxlestage/Nuage#in-a-window-nuage-db-editor"));
            }
            if ui.button(t(l, "Open the license")).clicked() {
                ctx.open_url(egui::OpenUrl::new_tab("https://github.com/maxlestage/Nuage/blob/master/LICENSE"));
            }
        });
    }

    /// The server installed on this computer.
    pub(super) fn computer_page(&mut self, ui: &mut egui::Ui, l: Lang) {
        ui.add_space(8.0);
        self.back_link(ui, l);
        ui.heading(t(l, "Nuage DB on this computer"));
        ui.add_space(6.0);
        let ctx = ui.ctx().clone();
        let Some(here) = self.here.clone() else {
            help(ui, t(l, "Looking for a Nuage DB server installed here…"));
            if self.here_job.is_none() {
                self.look_here(&ctx);
            }
            return;
        };
        let Some(found) = here.installed else {
            help(ui, t(l, "No server was installed on this computer by the installer."));
            return;
        };
        let mut start = false;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(match (found.paths.kind, found.service) {
                (_, false) if found.paths.kind == Kind::Linux => {
                    t(l, "This computer has no systemd: the installer left Nuage DB to be started by hand.")
                }
                (Kind::Linux, _) => t(l, "Starts with the computer: systemd service nuage."),
                (Kind::MacOs, _) => t(l, "Starts with the computer: launchd daemon io.github.maxlestage.nuage."),
                (Kind::Windows, _) => t(l, "Starts with the computer: scheduled task Nuage DB."),
            });
            if found.answers {
                theme::status(ui, Mark::Ok, tf(l, "Answers on port {}", &[&found.port]));
            } else {
                theme::status(ui, Mark::Bad, tf(l, "Does not answer on port {}", &[&found.port]));
            }
            if ui.button(t(l, "Check again")).clicked() {
                self.here = None;
                self.look_here(&ctx);
            }
            ui.add_space(6.0);
            for (what, path) in [(t(l, "Settings file"), Some(&found.paths.settings)), (t(l, "Log"), found.paths.log.as_ref())] {
                let Some(path) = path else { continue };
                ui.horizontal_wrapped(|ui| {
                    ui.label(what);
                    ui.monospace(path.display().to_string());
                    if ui.button(tf(l, "Copy · {}", &[&what])).clicked() {
                        ui.ctx().copy_text(path.display().to_string());
                    }
                });
            }
            help(ui, log_place(l));
            if !found.answers && found.service {
                ui.add_space(6.0);
                help(
                    ui,
                    t(
                        l,
                        "The editor starts the service; it never stops or restarts a running server. Your administrator password will be asked.",
                    ),
                );
                match &self.starting {
                    Some(starting) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.spinner();
                            ui.label(tf(
                                l,
                                "Starting… waiting for Nuage DB to answer ({} s; 60 s at most; it restores from its backup if its data folder is empty).",
                                &[&starting.job.elapsed().as_secs()],
                            ));
                            if !cfg!(windows) && ui.button(t(l, "Stop waiting")).clicked() {
                                starting.stop.now();
                            }
                        });
                    }
                    None => start = ui.add(theme::primary(t(l, "Start Nuage DB"), ui)).clicked(),
                }
            }
            if let Some(n) = &self.start_note {
                notice(ui, n);
            }
        });
        if start && self.local == LocalServer::Installed {
            let stop = local::Stop::default();
            let port = found.port;
            let job_stop = stop.clone();
            self.start_note = None;
            let job = Job::start(&ctx, &self.calls, move || {
                if let Err(e) = local::start_service(l, &job_stop) {
                    return Ok(Err(e));
                }
                for _ in 0..60 {
                    if local::health_answers(port) {
                        return Ok(Ok(true));
                    }
                    if job_stop.asked() {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                Ok(Ok(false))
            });
            self.starting = Some(Starting { job, stop });
        }
    }
}

/// Where this system's installer puts the server's log.
fn log_place(l: Lang) -> String {
    if cfg!(windows) {
        t(l, "Its log: C:\\ProgramData\\Nuage\\nuage.log, readable by administrators.").to_owned()
    } else if cfg!(target_os = "macos") {
        tf(l, "Its log: {}.", &[&local::MACOS_LOG])
    } else {
        t(l, "Its log is in the system journal, unit nuage (GNOME Logs shows it).").to_owned()
    }
}

/// Writes the editor's entry in the applications menu of a Linux desktop
/// (for this account), pointing at this program where it is.
#[cfg(all(unix, not(target_os = "macos")))]
fn add_to_menu() -> Result<std::path::PathBuf, String> {
    use std::path::PathBuf;
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or("no home folder (HOME is not set)")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exec = desktop_exec(&exe.to_string_lossy()).ok_or("the editor's path has a line break")?;
    let entry = include_str!("../../packaging/linux/io.github.maxlestage.nuage-editor.desktop")
        .replace("Exec=nuage-editor", &format!("Exec={exec}"));
    let apps = data.join("applications");
    let icons = data.join("icons/hicolor/256x256/apps");
    let file = apps.join(format!("{}.desktop", crate::APP_ID));
    std::fs::create_dir_all(&apps).map_err(|e| format!("{}: {e}", apps.display()))?;
    std::fs::create_dir_all(&icons).map_err(|e| format!("{}: {e}", icons.display()))?;
    std::fs::write(&file, entry).map_err(|e| format!("{}: {e}", file.display()))?;
    let icon = icons.join(format!("{}.png", crate::APP_ID));
    std::fs::write(&icon, include_bytes!("../../packaging/linux/io.github.maxlestage.nuage-editor.png"))
        .map_err(|e| format!("{}: {e}", icon.display()))?;
    Ok(file)
}

/// A path as the `Exec` key of a desktop entry quotes it.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
pub fn desktop_exec(path: &str) -> Option<String> {
    if path.contains(['\n', '\r']) {
        return None;
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        match c {
            // Escaped for the quoting, then that backslash for the string.
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_is_as_nuage_genkey_makes_them() {
        let a = genkey().unwrap();
        let b = genkey().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert!(a.expose().bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        assert_ne!(a, b);
        assert_eq!(desktop_exec("/home/me/nuage-editor").as_deref(), Some("\"/home/me/nuage-editor\""));
        assert_eq!(desktop_exec("/a b/100%$x").as_deref(), Some("\"/a b/100%%\\\\$x\""));
        assert_eq!(desktop_exec("/a\nb"), None);
    }
}
