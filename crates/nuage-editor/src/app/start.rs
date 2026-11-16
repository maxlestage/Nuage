//! The start: what the editor does, this computer's server opened with
//! nothing to type, a server elsewhere with its address and its token (and
//! where to find the token on each host), the servers used before, and how
//! to get a server.

use std::time::Duration;

use eframe::egui::{self, RichText};
use zeroize::Zeroizing;

use super::{Editor, Given, Host, Notice, SavedServer, Section, help, notice, token_field};
use crate::address::{self, AddressError};
use crate::api::{ApiError, Step};
use crate::i18n::{Lang, t, tf};
use crate::local;
use crate::theme::{self, Mark};

/// The installers' one line for this system (README.md, "Install on any computer").
#[cfg(windows)]
pub const INSTALL_COMMAND: &str = "irm https://raw.githubusercontent.com/maxlestage/Nuage/master/install.ps1 | iex";
#[cfg(not(windows))]
pub const INSTALL_COMMAND: &str =
    "curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh | sudo bash";

/// The installation guide.
pub const GUIDE: &str = "https://github.com/maxlestage/Nuage#install-on-any-computer";
/// Heroku's button, with the project's template.
pub const DEPLOY_TO_HEROKU: &str = "https://heroku.com/deploy?template=https://github.com/maxlestage/Nuage";
/// Where Heroku lists its apps.
pub const HEROKU_DASHBOARD: &str = "https://dashboard.heroku.com/apps";

impl Editor {
    pub(super) fn start_page(&mut self, ui: &mut egui::Ui, l: Lang) {
        let ctx = ui.ctx().clone();
        ui.add_space(12.0);
        ui.heading(t(l, "Welcome"));
        ui.label(t(
            l,
            "This editor looks after your Nuage DB server without a terminal: see that it works, create an application for your website, choose how much memory it uses.",
        ));
        ui.add_space(10.0);
        if self.connect.job.is_some() || self.connect.failed {
            self.connecting(ui, &ctx, l);
            ui.add_space(10.0);
        } else if let Some(n) = &self.connect.notice {
            notice(ui, n);
            ui.add_space(6.0);
        }
        if self.connect.asking {
            self.asking(ui, &ctx, l);
            ui.add_space(10.0);
        }
        self.this_computer(ui, &ctx, l);
        ui.add_space(10.0);
        self.elsewhere(ui, &ctx, l);
        self.servers_used(ui, &ctx, l);
        ui.add_space(10.0);
        no_server_yet(ui, l);
        ui.add_space(12.0);
        help(
            ui,
            t(l, "The editor changes nothing by itself: every change is a button you press, and it says what it did."),
        );
        ui.horizontal_wrapped(|ui| {
            if ui.button(t(l, "Tools")).clicked() {
                self.aside = Some(Section::Tools);
            }
            if ui.button(t(l, "About")).clicked() {
                self.aside = Some(Section::About);
            }
        });
    }

    /// The steps of a connection, and what to do when one failed.
    fn connecting(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang) {
        let running = self.connect.job.as_ref().map(|job| job.elapsed());
        let at = Step::of(self.connect.step.load(std::sync::atomic::Ordering::SeqCst));
        let mut stop = false;
        let mut again = false;
        let mut change = false;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Connecting"));
            if let Some(read) = &self.connect.read_note {
                theme::status(ui, Mark::Ok, read.clone());
            }
            let steps = if self.connect.stepped { 3 } else { 0 };
            for (step, text) in [
                (Step::Looking, t(l, "Looking for the server…")),
                (Step::Token, t(l, "Checking the token…")),
                (Step::Reading, t(l, "Reading the settings…")),
            ]
            .into_iter()
            .take(steps)
            {
                ui.horizontal(|ui| {
                    if step < at {
                        theme::mark(ui, Mark::Ok);
                        ui.label(text);
                    } else if step == at && running.is_some() {
                        ui.spinner();
                        ui.label(text);
                    } else if step == at {
                        theme::mark(ui, Mark::Bad);
                        ui.label(text);
                    } else {
                        theme::mark(ui, Mark::Off);
                        ui.label(RichText::new(text).weak());
                    }
                });
            }
            if let Some(elapsed) = running {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tf(l, "{} s", &[&elapsed.as_secs()])).weak());
                    if elapsed >= Duration::from_secs(3) {
                        stop = ui.button(t(l, "Stop waiting")).clicked();
                    }
                });
                if elapsed >= Duration::from_secs(8) {
                    help(ui, t(l, "A server that was asleep (Heroku Eco) can take half a minute to wake."));
                }
            }
            if running.is_none()
                && let Some(n) = &self.connect.notice
            {
                notice(ui, n);
                ui.horizontal_wrapped(|ui| {
                    again = ui.add(theme::primary(t(l, "Try again"), ui)).clicked();
                    change = ui.button(t(l, "Change the address or the token")).clicked();
                    if let Some(to) = self.connect.upgrade.clone()
                        && ui.button(tf(l, "Use {}", &[&to])).clicked()
                    {
                        self.connect.address = to;
                        self.connect.upgrade = None;
                        self.connect.failed = false;
                        self.connect.notice = None;
                    }
                });
            }
        });
        if stop {
            // The call ends by itself; its answer is dropped, and its token is not kept.
            self.connect.job = None;
            self.connect.with_kept = false;
            self.connect.with_local = false;
            self.connect.with_remember = false;
            self.connect.failed = true;
            self.connect.notice =
                Some(Notice::warn(t(l, "Stopped waiting. The server may still answer later; nothing was changed.")));
        }
        if again {
            self.connect.failed = false;
            match self.connect.last {
                Some(super::Attempt::Here) => self.read_here(ctx),
                // The system's window comes only after its explanation again.
                Some(super::Attempt::Administrator) => {
                    self.connect.notice = None;
                    self.connect.asking = self.connect.needs_admin.is_some();
                }
                _ => self.start_connect(ctx, if self.kept_here() { Given::Kept } else { Given::Typed }),
            }
        }
        if change {
            self.connect.failed = false;
            self.connect.notice = None;
            self.connect.read_note = None;
            self.connect.use_kept = false;
            self.connect.focus = Some(egui::Id::unique("nuage-editor-address"));
        }
    }

    /// Before the system's window: why an administrator is asked, and what is read.
    fn asking(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang) {
        let Some((file, can_ask)) = self.connect.needs_admin.clone() else {
            self.connect.asking = false;
            return;
        };
        let waiting = self.connect.local_job.as_ref().map(|j| j.elapsed());
        let mut go = false;
        let mut paste = false;
        let mut cancel = false;
        let mut stop = false;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Read the management token of Nuage DB"));
            match &can_ask {
                Ok(()) => {
                    ui.label(t(l, super::ASK_ADMINISTRATOR));
                    ui.label(t(
                        l,
                        "Only four lines are read: NUAGE_ADMIN_TOKEN, PORT, NUAGE_BIND and SITE_URL. The encryption key and the passwords are not read.",
                    ));
                    self.remember_box(ui, l);
                }
                Err(why) => {
                    theme::status(
                        ui,
                        Mark::Warn,
                        tf(
                            l,
                            "This desktop cannot show a password window ({}): paste the token instead; it is the line NUAGE_ADMIN_TOKEN= of {}.",
                            &[why, &file.display()],
                        ),
                    );
                }
            }
            match waiting {
                Some(elapsed) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.spinner();
                        ui.label(t(l, "Waiting for your answer in the system's window…"));
                        // pkexec and osascript can be stopped, and their window with them; Windows' cannot.
                        if cfg!(windows) {
                            help(ui, t(l, "Windows' window cannot be withdrawn from here: answer it, or close it."));
                        } else if elapsed >= Duration::from_secs(3) {
                            stop = ui.button(t(l, "Stop waiting")).clicked();
                        }
                    });
                }
                None => {
                    ui.horizontal_wrapped(|ui| {
                        if can_ask.is_ok() {
                            go = ui.add(theme::primary(t(l, "Continue"), ui)).clicked();
                        }
                        paste = ui.button(t(l, "I have the token, I'll paste it")).clicked();
                        cancel = ui.button(t(l, "Cancel")).clicked();
                    });
                }
            }
        });
        if go {
            self.read_as_administrator(ctx);
        }
        if stop && let Some(s) = &self.connect.local_stop {
            s.now();
        }
        if paste {
            let port = self.here.as_ref().and_then(|h| h.installed.as_ref()).map_or(local::DEFAULT_PORT, |d| d.port);
            self.connect.address = format!("http://127.0.0.1:{port}");
            self.connect.use_kept = false;
            self.connect.asking = false;
            self.connect.help = Some(Host::Installer);
            self.connect.focus = Some(token_field());
        }
        if cancel {
            self.connect.asking = false;
            // Ticked for this card only.
            self.connect.remember = false;
        }
    }

    /// A server of this computer: installed by the installer, or answering on 8080.
    fn this_computer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang) {
        let busy = self.connect.job.is_some() || self.connect.local_job.is_some();
        let here = self.here.clone();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "On this computer"));
            let Some(here) = here else {
                help(ui, t(l, "Looking for a Nuage DB server installed here…"));
                return;
            };
            if let Some(found) = &here.installed {
                // A token already kept for it: no prompt at all.
                let kept = self.prefs.servers.iter().find(|s| s.local && s.token_kept).cloned();
                match &kept {
                    Some(saved) => {
                        ui.label(tf(
                            l,
                            "Nuage DB on this computer — token kept in {}",
                            &[&self.vault.store().place(l)],
                        ));
                        if ui.add_enabled(!busy, theme::primary(t(l, "Open"), ui)).clicked() {
                            self.connect.address = saved.address.clone();
                            self.connect.use_kept = true;
                            self.start_connect(ctx, Given::Kept);
                        }
                    }
                    None => {
                        ui.label(t(l, "Nuage DB is installed on this computer."));
                        if found.readable {
                            help(ui, t(l, "Nothing to type."));
                            if ui.add_enabled(!busy, theme::primary(t(l, "Open"), ui)).clicked() {
                                self.read_here(ctx);
                            }
                        } else {
                            help(ui, t(l, ASK_ONCE));
                            if ui.add_enabled(!busy, theme::primary(t(l, "Open…"), ui)).clicked() {
                                self.read_here(ctx);
                            }
                        }
                    }
                }
                if !found.service && found.paths.kind != local::Kind::Windows {
                    theme::status(
                        ui,
                        Mark::Warn,
                        t(
                            l,
                            "Nuage DB seems installed, but its service is missing: installing it again repairs it; data and settings stay.",
                        ),
                    );
                }
                if found.answers {
                    theme::status(ui, Mark::Ok, tf(l, "Answers on port {}", &[&found.port]));
                } else {
                    theme::status(
                        ui,
                        Mark::Warn,
                        tf(
                            l,
                            "Nothing answers on port {} of this computer right now: the server may be stopped.",
                            &[&found.port],
                        ),
                    );
                    if kept.is_none() && ui.button(t(l, "This computer…")).clicked() {
                        self.aside = Some(Section::Computer);
                    }
                }
            } else if here.answers_8080 {
                ui.label(t(
                    l,
                    "Nuage DB answers on this computer (port 8080): Docker, or a server started by hand.",
                ));
                if ui.button(t(l, "Use this address")).clicked() {
                    self.connect.address = "http://127.0.0.1:8080".into();
                    self.connect.use_kept = false;
                    self.connect.help = Some(Host::Docker);
                    self.connect.focus = Some(token_field());
                }
            } else {
                help(ui, t(l, "No server was installed on this computer by the installer."));
            }
        });
    }

    /// A server elsewhere: its address and its management token.
    fn elsewhere(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang) {
        let kept_here = self.kept_here();
        let waiting = self.connect.job.is_some() || self.connect.local_job.is_some();
        let mut submit = false;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "My server is elsewhere: Heroku, a host, another computer"));
            let mut cleartext = None;
            let label = ui.strong(t(l, "Server address"));
            let id = egui::Id::unique("nuage-editor-address");
            let field = ui
                .add(
                    egui::TextEdit::singleline(&mut self.connect.address)
                        .id(id)
                        .hint_text("https://my-nuage.herokuapp.com  ·  localhost:8080")
                        .desired_width(420.0),
                )
                .labelled_by(label.id);
            if self.connect.focus == Some(id) {
                field.request_focus();
                self.connect.focus = None;
            }
            // What was said of the last try is out of date once the address changes.
            if field.changed() {
                self.connect.notice = None;
            }
            help(ui, t(l, "The address you type to open the console, without /admin."));
            // What will be called, before anything is.
            match address::normalize(&self.connect.address) {
                Ok(a) => {
                    let mut line = tf(l, "Will connect to {}", &[&a.base()]);
                    if let Some(dropped) = &a.dropped {
                        line = format!("{line} {}", tf(l, "({} dropped)", &[dropped]));
                    }
                    help(ui, line);
                    if a.is_cleartext_public() {
                        cleartext = Some(a.base());
                    }
                }
                Err(AddressError::Empty) => {}
                Err(e) => {
                    theme::status(ui, Mark::Warn, address_error(l, &self.connect.address, &e));
                }
            }
            ui.add_space(6.0);
            let label = ui.strong(t(l, "Management token"));
            if kept_here {
                ui.horizontal_wrapped(|ui| {
                    ui.label(tf(l, "Token kept in {}.", &[&self.vault.store().place(l)]));
                    if let Some(base) = self.typed_base()
                        && ui.add_enabled(self.forget_job.is_none(), egui::Button::new(t(l, "Forget this token"))).clicked()
                    {
                        self.forget(ctx, base, false);
                    }
                    if ui.button(t(l, "Type another token")).clicked() {
                        self.connect.use_kept = false;
                    }
                });
            } else {
                // Show goes under the field when the window is narrow.
                ui.horizontal_wrapped(|ui| {
                    let field = ui
                        .add(
                            egui::TextEdit::singleline(&mut *self.connect.token)
                                .id(token_field())
                                .password(!self.connect.show_token)
                                .hint_text("NUAGE_ADMIN_TOKEN")
                                .desired_width(340.0),
                        )
                        .labelled_by(label.id);
                    if self.connect.focus == Some(token_field()) {
                        field.request_focus();
                        self.connect.focus = None;
                    }
                    if field.changed() {
                        self.connect.notice = None;
                    }
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    ui.checkbox(&mut self.connect.show_token, t(l, "Show"));
                });
                help(
                    ui,
                    t(
                        l,
                        "The value of NUAGE_ADMIN_TOKEN on the server (64 characters when the installer or Heroku made it). Not the console password, not an application's token (nuage_…).",
                    ),
                );
                self.where_is_it(ui, l);
                self.remember_box(ui, l);
            }
            if let Some(base) = &cleartext {
                theme::status(
                    ui,
                    Mark::Bad,
                    t(l, "Not encrypted: the token would cross the Internet in clear. Use the server's https:// address."),
                );
                let mut anyway = self.connect.cleartext_ok.as_deref() == Some(base.as_str());
                if ui.checkbox(&mut anyway, t(l, "Connect anyway")).changed() {
                    self.connect.cleartext_ok = anyway.then(|| base.clone());
                }
            }
            let consented = cleartext.is_none() || self.connect.cleartext_ok == cleartext;
            ui.add_space(4.0);
            if ui.add_enabled(!waiting && consented, theme::primary(t(l, "Connect"), ui)).clicked() {
                submit = true;
            }
            if !consented {
                submit = false;
            }
        });
        if submit && !waiting {
            self.connect.read_note = None;
            self.start_connect(ctx, if kept_here { Given::Kept } else { Given::Typed });
        }
    }

    /// "Where do I find it?", by host.
    fn where_is_it(&mut self, ui: &mut egui::Ui, l: Lang) {
        let open = self.connect.help.is_some();
        let header =
            egui::CollapsingHeader::new(t(l, "Where do I find it?")).id_salt("where-is-it").open(open.then_some(true));
        let shown = header.show(ui, |ui| {
            let mut host = self.connect.help.unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut host, Host::Heroku, "Heroku");
                ui.selectable_value(&mut host, Host::Installer, t(l, "Installed with the installer"));
                ui.selectable_value(&mut host, Host::Docker, "Docker");
            });
            self.connect.help = Some(host);
            match host {
                Host::Heroku => {
                    ui.label(t(
                        l,
                        "1. Open your app in the Heroku dashboard. 2. Settings, then Reveal Config Vars. 3. Copy the value of NUAGE_ADMIN_TOKEN.",
                    ));
                    if ui.button(t(l, "Open the Heroku dashboard")).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(HEROKU_DASHBOARD));
                    }
                    help(
                        ui,
                        t(
                            l,
                            "No NUAGE_ADMIN_TOKEN there (a deploy from before 0.4.2)? Add one: name NUAGE_ADMIN_TOKEN, value from Generate a token. Heroku restarts the server by itself (about a minute).",
                        ),
                    );
                }
                Host::Installer => {
                    ui.label(t(
                        l,
                        "On the server's computer, in its settings file, the line NUAGE_ADMIN_TOKEN= (Linux /etc/nuage/nuage.env, Mac /usr/local/etc/nuage/nuage.env, Windows C:\\ProgramData\\Nuage\\nuage.env); only an administrator of that computer can open it. Simplest: open the editor on that computer once.",
                    ));
                }
                Host::Docker => {
                    ui.label(t(
                        l,
                        "In the .env file next to docker-compose.yml, the line NUAGE_ADMIN_TOKEN=. Without one: Generate a token, add the line, then restart the container.",
                    ));
                }
            }
            if host != Host::Installer {
                super::tools::generator(ui, &mut self.generated, self.clipboard.as_mut(), l);
            }
        });
        if shown.header_response.clicked() {
            self.connect.help = if open { None } else { Some(self.connect.help.unwrap_or_default()) };
        }
    }

    /// "Remember on this computer", when the system has a store the editor can use.
    pub(super) fn remember_box(&mut self, ui: &mut egui::Ui, l: Lang) {
        let store = self.vault.store().place(l);
        match &self.vault_status {
            Some(Ok(())) => {
                ui.checkbox(&mut self.connect.remember, t(l, "Remember on this computer"));
                if self.connect.remember {
                    help(ui, tf(l, "The token goes in {}.", &[&store]));
                }
            }
            Some(Err(why)) => {
                let mut no = false;
                ui.add_enabled(false, egui::Checkbox::new(&mut no, t(l, "Remember on this computer")));
                help(
                    ui,
                    t(
                        l,
                        "Your system has no password store the editor can use: the token is kept until you close the editor.",
                    ),
                );
                // The system's own words, for whoever helps: not in the sentence.
                egui::CollapsingHeader::new(t(l, "Details")).id_salt("vault-details").show(ui, |ui| {
                    help(ui, why.to_string());
                });
            }
            None => {
                let mut no = false;
                ui.add_enabled(false, egui::Checkbox::new(&mut no, t(l, "Remember on this computer")));
                let waited = self.vault_job.as_ref().is_some_and(|j| j.elapsed() >= Duration::from_secs(2));
                help(
                    ui,
                    if waited {
                        t(l, "Waiting for your keyring…")
                    } else {
                        t(l, "Looking for this system's password store…")
                    },
                );
            }
        }
        help(ui, t(l, "This token can make tokens for every application: remember it only on your own computer."));
    }

    /// Forgets the token kept for `address`, in the password store; `drop`:
    /// and its server from the list, once the store has deleted it (else
    /// nothing would be left to try again).
    pub(super) fn forget(&mut self, ctx: &egui::Context, address: String, drop: bool) {
        let vault = self.vault.clone();
        self.forget_job = Some(super::Job::start(ctx, &self.calls, move || {
            let done = vault.delete(&address);
            Ok((address, drop, done))
        }));
    }

    /// The servers connected to before.
    fn servers_used(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang) {
        if self.prefs.servers.is_empty() {
            return;
        }
        ui.add_space(10.0);
        let waiting = self.connect.job.is_some() || self.connect.local_job.is_some();
        let mut chosen = None;
        let mut forget = None;
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            theme::subheading(ui, t(l, "Servers you used"));
            for saved in &self.prefs.servers {
                ui.horizontal_wrapped(|ui| {
                    // The address connects; a screen reader says what each one does, and to which server.
                    let link = ui
                        .add_enabled(!waiting, egui::Link::new(RichText::new(&saved.address).monospace()))
                        .accessible_name(tf(l, "Connect to {}", &[&saved.address]));
                    if link.on_hover_text(t(l, "Connect")).clicked() {
                        chosen = Some(saved.clone());
                    }
                    if saved.token_kept {
                        ui.label(RichText::new(t(l, "token kept")).weak());
                    } else if saved.local {
                        ui.label(RichText::new(t(l, "read from this computer")).weak());
                    }
                    let forget_button = egui::Button::new(t(l, "Forget")).small();
                    let forget_button = ui.add_enabled(self.forget_job.is_none(), forget_button).accessible_name(tf(
                        l,
                        "Forget {}",
                        &[&saved.address],
                    ));
                    if forget_button.clicked() {
                        forget = Some(saved.clone());
                    }
                });
            }
        });
        if let Some(saved) = chosen {
            self.connect.address = saved.address.clone();
            self.connect.use_kept = saved.token_kept;
            self.connect.token = Zeroizing::new(String::new());
            self.connect.cleartext_ok = None;
            self.connect.upgrade = None;
            self.connect.notice = None;
            self.connect.read_note = None;
            if saved.token_kept {
                self.start_connect(ctx, Given::Kept);
            } else if saved.local {
                self.read_here(ctx);
            } else {
                self.connect.focus = Some(token_field());
            }
        }
        if forget.is_some() {
            self.forget_ask = forget;
        }
    }

    /// The question before a saved server is forgotten, and before everything is.
    pub(super) fn forget_dialogs(&mut self, ctx: &egui::Context, l: Lang) {
        if let Some(saved) = self.forget_ask.clone() {
            let mut answer = None;
            let modal = egui::Modal::new(egui::Id::unique("forget-server")).show(ctx, |ui| {
                ui.set_max_width(460.0);
                theme::subheading(ui, t(l, "Forget this server?"));
                ui.label(if saved.token_kept {
                    tf(
                        l,
                        "Forget {}? Its token kept in {} is deleted.",
                        &[&saved.address, &self.vault.store().place(l)],
                    )
                } else {
                    tf(l, "Forget {}?", &[&saved.address])
                });
                ui.horizontal(|ui| {
                    if ui.add(theme::danger(t(l, "Forget"), ui)).clicked() {
                        answer = Some(true);
                    }
                    if ui.button(t(l, "Keep it")).clicked() {
                        answer = Some(false);
                    }
                });
            });
            if modal.should_close() {
                answer = Some(false);
            }
            if let Some(yes) = answer {
                self.forget_ask = None;
                match (yes, saved.token_kept) {
                    // Out of the list once the store has deleted its token.
                    (true, true) => self.forget(ctx, saved.address, true),
                    (true, false) => self.prefs.servers.retain(|s| s.address != saved.address),
                    (false, _) => {}
                }
            }
        }
        if self.forget_all_ask {
            let mut answer = None;
            let modal = egui::Modal::new(egui::Id::unique("forget-everything")).show(ctx, |ui| {
                ui.set_max_width(460.0);
                theme::subheading(ui, t(l, "Forget everything on this computer?"));
                ui.label(tf(
                    l,
                    "The servers you used and the language and theme chosen are forgotten, and every token the editor kept in {} is deleted. Your servers are not changed.",
                    &[&self.vault.store().place(l)],
                ));
                ui.horizontal(|ui| {
                    if ui.add(theme::danger(t(l, "Forget everything"), ui)).clicked() {
                        answer = Some(true);
                    }
                    if ui.button(t(l, "Keep it")).clicked() {
                        answer = Some(false);
                    }
                });
            });
            if modal.should_close() {
                answer = Some(false);
            }
            if let Some(yes) = answer {
                self.forget_all_ask = false;
                if yes {
                    let mut kept: Vec<String> =
                        self.prefs.servers.iter().filter(|s| s.token_kept).map(|s| s.address.clone()).collect();
                    // A token being written goes too; a connection on its way would put its
                    // server back in the list (and keep its token): its answer is dropped.
                    for keeping in std::mem::take(&mut self.keeping) {
                        keeping.forgotten.store(true, std::sync::atomic::Ordering::SeqCst);
                        kept.push(keeping.address);
                    }
                    self.connect.job = None;
                    self.connect.with_kept = false;
                    self.connect.with_local = false;
                    self.connect.with_remember = false;
                    self.prefs = super::Prefs::default();
                    self.session = None;
                    let vault = self.vault.clone();
                    self.forget_all_job = Some(super::Job::start(ctx, &self.calls, move || {
                        Ok(kept.into_iter().map(|a| (a.clone(), vault.delete(&a))).collect())
                    }));
                }
            }
        }
    }

    /// The question when the window is closed while something would be lost.
    pub(super) fn quit_dialog(&mut self, ctx: &egui::Context, l: Lang) {
        if !self.quit_ask {
            return;
        }
        let Some(session) = &self.session else {
            self.quit_ask = false;
            return;
        };
        let writing = session.busy();
        let address = session.server.address().to_owned();
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("quit")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            if writing {
                ui.label(tf(
                    l,
                    "A change is being sent to {}. If you quit now it may or may not be carried out.",
                    &[&address],
                ));
            } else {
                ui.label(t(l, "A new token is shown and has not been copied: it cannot be shown again."));
            }
            ui.horizontal(|ui| {
                let back = if writing { t(l, "Wait") } else { t(l, "Back to it") };
                if ui.add(theme::primary(back, ui)).clicked() {
                    answer = Some(false);
                }
                if ui.button(t(l, "Quit anyway")).clicked() {
                    answer = Some(true);
                }
            });
        });
        if let Some(quit) = answer {
            self.quit_ask = false;
            if quit {
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

/// How to get a server, folded.
fn no_server_yet(ui: &mut egui::Ui, l: Lang) {
    egui::CollapsingHeader::new(t(l, "I don't have a server yet")).id_salt("no-server-yet").show(ui, |ui| {
        ui.label(t(
            l,
            "Installing Nuage DB on a computer takes one command, pasted once into Terminal (Mac, Linux) or into PowerShell opened as administrator (Windows). Online, the Deploy to Heroku button needs no command.",
        ));
        ui.label(RichText::new(INSTALL_COMMAND).monospace());
        ui.horizontal_wrapped(|ui| {
            if ui.button(t(l, "Copy the command for this computer")).clicked() {
                ui.ctx().copy_text(INSTALL_COMMAND.to_owned());
            }
            if ui.button(t(l, "Open the installation guide")).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(GUIDE));
            }
            if ui.button(t(l, "Open Deploy to Heroku")).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(DEPLOY_TO_HEROKU));
            }
        });
        help(ui, t(l, "Then come back here."));
    });
}

/// What the "On this computer" card says before the system asks for an administrator.
pub(super) const ASK_LINUX: &str = "Your password will be asked once, to read its management token.";
pub(super) const ASK_MACOS: &str = "This Mac will ask for an administrator's name and password.";
pub(super) const ASK_WINDOWS: &str =
    "Windows will ask an administrator's permission once, to read its management token.";
const ASK_ONCE: &str = if cfg!(windows) {
    ASK_WINDOWS
} else if cfg!(target_os = "macos") {
    ASK_MACOS
} else {
    ASK_LINUX
};

/// Why `typed` is not an address, said before anything is sent.
pub(super) fn address_error(l: Lang, typed: &str, e: &AddressError) -> String {
    match e {
        AddressError::Empty => t(l, "Type the address of the server first.").to_owned(),
        AddressError::Scheme(scheme) => tf(l, "The editor speaks http:// and https:// only, not {}://.", &[scheme]),
        AddressError::Host(_) => ApiError::BadAddress(typed.trim().to_owned()).explain(l),
        AddressError::Port(port) => tf(l, "{} is not a port: a port is a number from 1 to 65535.", &[port]),
    }
}

impl SavedServer {
    /// A saved server at `address`, its token not kept.
    pub fn at(address: &str) -> SavedServer {
        SavedServer { address: address.to_owned(), ..SavedServer::default() }
    }
}
