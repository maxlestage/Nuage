//! The window: connecting to a server, then its applications, its memory,
//! its limits and what it was started with.

use std::collections::BTreeMap;

use eframe::egui::{self, Color32, RichText};

use crate::api::{ApiError, Application, Connection, Server, Settings};
use crate::format;
use crate::i18n::{Lang, t, tf};
use crate::jobs::{Job, take};
use crate::local;
use crate::store::Tokens;

/// What eframe keeps between two launches.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The language chosen (`None`: the system's).
    pub lang: Option<Lang>,
    pub theme: Theme,
    /// The servers connected to, the last one first (addresses only: a token
    /// is remembered apart, when its owner asks).
    pub servers: Vec<String>,
}

/// Light, dark, or as the system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// The parts of a connected server's window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Overview,
    Applications,
    Memory,
    Limits,
    Startup,
}

impl Section {
    const ALL: [Section; 5] =
        [Section::Overview, Section::Applications, Section::Memory, Section::Limits, Section::Startup];

    fn label(self) -> &'static str {
        match self {
            Section::Overview => "Overview",
            Section::Applications => "Applications",
            Section::Memory => "Memory",
            Section::Limits => "Limits and visibility",
            Section::Startup => "Started with",
        }
    }
}

/// A message above the page: what went well, or what went wrong and what to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub ok: bool,
    pub text: String,
}

impl Notice {
    fn ok(text: impl Into<String>) -> Notice {
        Notice { ok: true, text: text.into() }
    }

    fn bad(text: impl Into<String>) -> Notice {
        Notice { ok: false, text: text.into() }
    }
}

/// What the connection form holds.
#[derive(Default)]
struct ConnectForm {
    address: String,
    token: String,
    show_token: bool,
    remember: bool,
    job: Option<Job<(Server, Settings)>>,
    local_job: Option<Job<local::Found>>,
    notice: Option<Notice>,
    /// The settings file of a server of this computer that needs the administrator's password.
    needs_admin: Option<std::path::PathBuf>,
}

/// What a change sent to the server comes back with.
enum Outcome {
    Settings(Settings),
    Created(Application, Connection),
    Token { app: String, read_only: bool, connection: Connection },
    Revoked { app: String, id: String },
    Memory(Application),
}

/// A token shown once, in a window of its own, until its owner closes it.
struct Reveal {
    app: String,
    read_only: bool,
    connection: Connection,
}

/// A token about to be revoked: its owner confirms.
struct Confirm {
    app: String,
    id: String,
}

/// A server the editor is connected to.
struct Session {
    server: Server,
    section: Section,
    settings: Option<Settings>,
    apps: Option<Vec<Application>>,
    load_settings: Option<Job<Settings>>,
    load_apps: Option<Job<Vec<Application>>>,
    /// One change at a time.
    action: Option<Job<Outcome>>,
    notice: Option<Notice>,
    /// The fields of the settings, as typed (filled from the server's values).
    fields: BTreeMap<String, String>,
    public: bool,
    /// The memory fields of each application, as typed.
    app_fields: BTreeMap<String, (String, String)>,
    new_name: String,
    new_description: String,
    reveal: Option<Reveal>,
    confirm: Option<Confirm>,
}

impl Session {
    fn new(server: Server, settings: Settings, lang: Lang) -> Session {
        let mut session = Session {
            server,
            section: Section::Overview,
            settings: None,
            apps: None,
            load_settings: None,
            load_apps: None,
            action: None,
            notice: None,
            fields: BTreeMap::new(),
            public: false,
            app_fields: BTreeMap::new(),
            new_name: String::new(),
            new_description: String::new(),
            reveal: None,
            confirm: None,
        };
        session.take_settings(settings, lang);
        session
    }

    /// The server's settings are in: the fields show them.
    fn take_settings(&mut self, settings: Settings, lang: Lang) {
        self.fields.clear();
        for s in &settings.settings {
            let shown = s.shown();
            let shown = if s.key.ends_with("_mb") && shown != "auto" {
                shown.parse::<u64>().map_or(shown.clone(), |mib| format::amount(lang, Some(mib)))
            } else {
                shown
            };
            self.fields.insert(s.key.clone(), shown);
        }
        self.public = settings.get("status.public").is_some_and(|s| s.shown() == "true");
        for a in &settings.applications {
            self.app_fields.insert(a.name.clone(), (format::amount(lang, a.cache_mb), format::amount(lang, a.kv_mb)));
        }
        self.settings = Some(settings);
    }

    fn reload(&mut self, ctx: &egui::Context) {
        if self.load_settings.is_none() {
            let server = self.server.clone();
            self.load_settings = Some(Job::start(ctx, move || server.settings()));
        }
        if self.load_apps.is_none() {
            let server = self.server.clone();
            self.load_apps = Some(Job::start(ctx, move || server.applications()));
        }
    }

    fn busy(&self) -> bool {
        self.action.is_some()
    }

    fn act(&mut self, ctx: &egui::Context, work: impl FnOnce(Server) -> Result<Outcome, ApiError> + Send + 'static) {
        if self.busy() {
            return;
        }
        let server = self.server.clone();
        self.notice = None;
        self.action = Some(Job::start(ctx, move || work(server)));
    }
}

/// The editor's window.
pub struct Editor {
    pub prefs: Prefs,
    tokens: Tokens,
    /// The settings file of a server of this computer, if one is installed.
    installed: Option<std::path::PathBuf>,
    connect: ConnectForm,
    session: Option<Session>,
    /// The theme the window was last given.
    theme_applied: Option<Theme>,
    /// Connect at the first frame (the address and the token came from the environment).
    auto_connect: bool,
}

impl Editor {
    /// The editor with what was remembered (`prefs`), and the remembered
    /// tokens kept in `dir` (`None`: none, as in the tests).
    pub fn new(prefs: Prefs, dir: Option<std::path::PathBuf>) -> Editor {
        let tokens = Tokens::open(dir);
        let mut connect = ConnectForm::default();
        if let Some(last) = prefs.servers.first() {
            connect.address = last.clone();
            if let Some(token) = tokens.get(last) {
                connect.token = token.to_owned();
                connect.remember = true;
            }
        }
        // Started with NUAGE_URL and NUAGE_ADMIN_TOKEN (a shortcut, a script):
        // the editor connects at once.
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let auto_connect = match (var("NUAGE_URL"), var("NUAGE_ADMIN_TOKEN")) {
            (Some(url), Some(token)) => {
                connect.address = url;
                connect.token = token;
                true
            }
            _ => false,
        };
        Editor {
            prefs,
            tokens,
            installed: local::installed(),
            connect,
            session: None,
            theme_applied: None,
            auto_connect,
        }
    }

    /// The language in use: the one chosen, else the system's.
    pub fn lang(&self) -> Lang {
        self.prefs.lang.unwrap_or_else(Lang::of_system)
    }

    /// The message the window shows above its page, if any.
    pub fn notice(&self) -> Option<&Notice> {
        match &self.session {
            Some(session) => session.notice.as_ref(),
            None => self.connect.notice.as_ref(),
        }
    }

    /// The address and the token typed in the connection form.
    #[doc(hidden)]
    pub fn typed(&self) -> (&str, &str) {
        (&self.connect.address, &self.connect.token)
    }

    /// The address of the server connected to.
    pub fn connected_to(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.server.address())
    }

    /// The whole window.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // Only when it changes: setting it asks for another frame.
        if self.theme_applied != Some(self.prefs.theme) {
            ctx.set_theme(match self.prefs.theme {
                Theme::System => egui::ThemePreference::System,
                Theme::Light => egui::ThemePreference::Light,
                Theme::Dark => egui::ThemePreference::Dark,
            });
            self.theme_applied = Some(self.prefs.theme);
        }
        if std::mem::take(&mut self.auto_connect) {
            self.start_connect(&ctx);
        }
        self.poll(&ctx);
        let l = self.lang();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, l));
        if self.session.is_some() {
            egui::Panel::left("sections").resizable(false).show(ui, |ui| self.sections(ui, l));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.session.as_mut() {
                Some(session) => session_page(ui, session, l),
                None => self.connect_page(ui, l),
            });
        });
        if let Some(session) = self.session.as_mut() {
            dialogs(&ctx, session, l);
        }
    }

    /// Takes the answers of the calls that came back.
    fn poll(&mut self, ctx: &egui::Context) {
        let l = self.lang();
        if let Some(answer) = take(&mut self.connect.job) {
            match answer {
                Ok((server, settings)) => {
                    let address = server.address().to_owned();
                    self.prefs.servers.retain(|s| *s != address);
                    self.prefs.servers.insert(0, address.clone());
                    self.prefs.servers.truncate(8);
                    let token = self.connect.remember.then_some(self.connect.token.trim());
                    if let Err(e) = self.tokens.set(&address, token) {
                        self.connect.notice = Some(Notice::bad(tf(l, "The token could not be remembered: {}", &[&e])));
                    }
                    let mut session = Session::new(server, settings, l);
                    session.reload(ctx);
                    self.session = Some(session);
                }
                Err(e) => self.connect.notice = Some(Notice::bad(explain(l, &e))),
            }
        }
        if let Some(answer) = take(&mut self.connect.local_job) {
            match answer {
                Ok(found) => {
                    self.connect.address = found.address.clone();
                    self.connect.needs_admin = None;
                    match found.token {
                        Some(token) => {
                            self.connect.token = token;
                            self.start_connect(ctx);
                        }
                        None => {
                            self.connect.notice = Some(Notice::bad(t(
                                l,
                                "The settings file of this computer has no management token (NUAGE_ADMIN_TOKEN): run the installer again, it adds one.",
                            )))
                        }
                    }
                }
                Err(e) => self.connect.notice = Some(Notice::bad(e.detail().to_owned())),
            }
        }
        let Some(session) = self.session.as_mut() else { return };
        if let Some(answer) = take(&mut session.load_settings) {
            match answer {
                Ok(settings) => session.take_settings(settings, l),
                Err(e) => session.notice = Some(Notice::bad(explain(l, &e))),
            }
        }
        if let Some(answer) = take(&mut session.load_apps) {
            match answer {
                Ok(apps) => session.apps = Some(apps),
                Err(e) => session.notice = Some(Notice::bad(explain(l, &e))),
            }
        }
        if let Some(answer) = take(&mut session.action) {
            match answer {
                Ok(outcome) => {
                    let mut notice = match outcome {
                        Outcome::Settings(settings) => {
                            let warning = settings.warning.clone();
                            session.take_settings(settings, l);
                            let mut n = Notice::ok(t(l, "Saved: in force now, with no restart."));
                            if let Some(w) = warning {
                                n = Notice::bad(format!("{} {w}", t(l, "Saved and in force, but:")));
                            }
                            n
                        }
                        Outcome::Created(app, connection) => {
                            session.new_name.clear();
                            session.new_description.clear();
                            session.reveal = Some(Reveal { app: app.name.clone(), read_only: false, connection });
                            Notice::ok(tf(l, "Application {} created, with its own encrypted database.", &[&app.name]))
                        }
                        Outcome::Token { app, read_only, connection } => {
                            session.reveal = Some(Reveal { app: app.clone(), read_only, connection });
                            Notice::ok(tf(l, "New token for {}.", &[&app]))
                        }
                        Outcome::Revoked { app, id } => {
                            Notice::ok(tf(l, "Token {}… of {} revoked: it opens nothing any more.", &[&id, &app]))
                        }
                        Outcome::Memory(app) => Notice::ok(tf(l, "Memory of {} saved: in force now.", &[&app.name])),
                    };
                    if notice.text.is_empty() {
                        notice = Notice::ok(t(l, "Done."));
                    }
                    session.notice = Some(notice);
                    session.reload(ctx);
                }
                Err(e) => session.notice = Some(Notice::bad(explain(l, &e))),
            }
        }
    }

    fn start_connect(&mut self, ctx: &egui::Context) {
        let l = self.lang();
        let address = self.connect.address.trim().to_owned();
        let token = self.connect.token.trim().to_owned();
        if address.is_empty() {
            self.connect.notice = Some(Notice::bad(t(l, "Type the address of the server first.")));
            return;
        }
        if token.is_empty() {
            self.connect.notice = Some(Notice::bad(t(l, "Type its management token first.")));
            return;
        }
        self.connect.notice = None;
        let server = Server::new(&address, &token, l);
        self.connect.job = Some(Job::start(ctx, move || server.settings().map(|s| (server, s))));
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, l: Lang) {
        ui.horizontal_wrapped(|ui| {
            ui.heading(RichText::new("Nuage DB").strong());
            ui.label(RichText::new(t(l, "Editor")).weak());
            ui.separator();
            let connected = self.session.as_ref().map(|s| s.server.address().to_owned());
            match &connected {
                Some(address) => {
                    dot(ui, Some(Color32::from_rgb(4, 150, 87)));
                    ui.label(tf(l, "Connected to {}", &[address]));
                }
                None => {
                    dot(ui, None);
                    ui.label(t(l, "Not connected"));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if connected.is_some() && ui.button(t(l, "Disconnect")).clicked() {
                    self.session = None;
                }
                ui.name_inputs(t(l, "Theme"), |ui| {
                    egui::ComboBox::from_id_salt("theme").selected_text(theme_label(l, self.prefs.theme)).show_ui(
                        ui,
                        |ui| {
                            for theme in [Theme::System, Theme::Light, Theme::Dark] {
                                ui.selectable_value(&mut self.prefs.theme, theme, theme_label(l, theme));
                            }
                        },
                    );
                });
                ui.name_inputs(t(l, "Language"), |ui| {
                    egui::ComboBox::from_id_salt("lang").selected_text(lang_label(l, self.prefs.lang)).show_ui(
                        ui,
                        |ui| {
                            ui.selectable_value(&mut self.prefs.lang, None, lang_label(l, None));
                            for lang in Lang::ALL {
                                ui.selectable_value(&mut self.prefs.lang, Some(lang), lang_label(l, Some(lang)));
                            }
                        },
                    );
                });
            });
        });
    }

    fn sections(&mut self, ui: &mut egui::Ui, l: Lang) {
        let Some(session) = self.session.as_mut() else { return };
        ui.add_space(8.0);
        for section in Section::ALL {
            if ui.selectable_label(session.section == section, t(l, section.label())).clicked() {
                session.section = section;
                session.notice = None;
            }
        }
        ui.add_space(12.0);
        ui.separator();
        if ui.button(t(l, "Open the console in my browser")).clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(session.server.console_url()));
        }
        if ui.button(t(l, "Refresh")).clicked() {
            session.reload(ui.ctx());
        }
    }

    fn connect_page(&mut self, ui: &mut egui::Ui, l: Lang) {
        let ctx = ui.ctx().clone();
        ui.add_space(12.0);
        ui.heading(t(l, "Connect to your Nuage DB server"));
        ui.label(t(
            l,
            "The editor changes your server's settings and applications, as the console does, without a terminal. It needs the address of the server and its management token.",
        ));
        ui.add_space(12.0);
        let waiting = self.connect.job.is_some() || self.connect.local_job.is_some();

        // A server of this computer, installed by the installer.
        egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
            ui.strong(t(l, "On this computer"));
            match self.installed.clone() {
                Some(file) => {
                    ui.label(tf(
                        l,
                        "A Nuage DB server is installed here (its settings are in {}).",
                        &[&file.display()],
                    ));
                    if let Some(needs) = self.connect.needs_admin.clone() {
                        if local::can_ask_administrator() {
                            ui.label(t(
                                l,
                                "Its settings file is protected (it holds the encryption key): your system asks for your password to read its address and its management token, and nothing else.",
                            ));
                            if ui.add_enabled(!waiting, egui::Button::new(t(l, "Enter my password"))).clicked() {
                                self.connect.local_job = Some(Job::start(&ctx, move || {
                                    local::read_as_administrator(&needs).map_err(|e| local_error(l, e))
                                }));
                            }
                        } else {
                            ui.label(t(
                                l,
                                "Its settings file is protected (it holds the encryption key). Close the editor, then open it again as administrator (right-click: Run as administrator) to read it once, with \"Remember on this computer\" ticked.",
                            ));
                        }
                    } else if ui.add_enabled(!waiting, egui::Button::new(t(l, "Use this server"))).clicked() {
                        match local::read() {
                            Ok(found) => {
                                self.connect.address = found.address.clone();
                                match found.token {
                                    Some(token) => {
                                        self.connect.token = token;
                                        self.start_connect(&ctx);
                                    }
                                    None => {
                                        self.connect.notice = Some(Notice::bad(t(
                                            l,
                                            "The settings file of this computer has no management token (NUAGE_ADMIN_TOKEN): run the installer again, it adds one.",
                                        )))
                                    }
                                }
                            }
                            Err(local::ReadError::NeedsAdministrator(file)) => self.connect.needs_admin = Some(file),
                            Err(e) => self.connect.notice = Some(Notice::bad(local_error(l, e).detail().to_owned())),
                        }
                    }
                }
                None => {
                    ui.label(t(
                        l,
                        "No server was installed on this computer by the installer. If yours is elsewhere (online, another computer), type its address below.",
                    ));
                }
            }
        });
        ui.add_space(10.0);

        // Any server: its address and its management token.
        egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
            ui.strong(t(l, "Address and management token"));
            egui::Grid::new("connect").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                let label = ui.label(t(l, "Address of the server"));
                ui.add(
                    egui::TextEdit::singleline(&mut self.connect.address)
                        .hint_text("https://my-nuage.herokuapp.com  ·  localhost:8080")
                        .desired_width(380.0),
                )
                .labelled_by(label.id);
                ui.end_row();
                let label = ui.label(t(l, "Management token"));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.connect.token)
                            .password(!self.connect.show_token)
                            .hint_text("NUAGE_ADMIN_TOKEN")
                            .desired_width(300.0),
                    )
                    .labelled_by(label.id);
                    ui.checkbox(&mut self.connect.show_token, t(l, "Show"));
                });
                ui.end_row();
            });
            ui.label(
                RichText::new(t(
                    l,
                    "The management token is NUAGE_ADMIN_TOKEN: in the settings file the installer wrote, or in your host's variables (Heroku: Settings, Config Vars). It is not an application's token.",
                ))
                .weak(),
            );
            ui.checkbox(&mut self.connect.remember, t(l, "Remember on this computer"));
            ui.horizontal(|ui| {
                if ui.add_enabled(!waiting, egui::Button::new(t(l, "Connect"))).clicked() {
                    self.start_connect(&ctx);
                }
                if waiting {
                    ui.spinner();
                    ui.label(t(l, "Checking…"));
                }
            });
        });

        if let Some(n) = &self.connect.notice {
            notice(ui, n);
        }

        // The servers connected to before.
        if !self.prefs.servers.is_empty() {
            ui.add_space(12.0);
            ui.strong(t(l, "Servers you used"));
            let mut chosen = None;
            let mut forget = None;
            for address in &self.prefs.servers {
                ui.horizontal(|ui| {
                    if ui.link(address).clicked() {
                        chosen = Some(address.clone());
                    }
                    if self.tokens.get(address).is_some() {
                        ui.label(RichText::new(t(l, "token remembered")).weak());
                    }
                    if ui.small_button(t(l, "Forget")).clicked() {
                        forget = Some(address.clone());
                    }
                });
            }
            if let Some(address) = chosen {
                self.connect.token = self.tokens.get(&address).unwrap_or_default().to_owned();
                self.connect.remember = self.tokens.get(&address).is_some();
                self.connect.address = address;
            }
            if let Some(address) = forget {
                self.prefs.servers.retain(|s| *s != address);
                let _ = self.tokens.set(&address, None);
            }
        }
    }
}

/// A small round mark: filled (connected), or a ring (not connected). Drawn,
/// not a character: the editor's font has no such glyph.
fn dot(ui: &mut egui::Ui, filled: Option<Color32>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    match filled {
        Some(color) => ui.painter().circle_filled(rect.center(), 5.0, color),
        None => ui.painter().circle_stroke(rect.center(), 4.5, ui.visuals().widgets.inactive.fg_stroke),
    };
}

fn theme_label(l: Lang, theme: Theme) -> &'static str {
    match theme {
        Theme::System => t(l, "Theme: as the system"),
        Theme::Light => t(l, "Theme: light"),
        Theme::Dark => t(l, "Theme: dark"),
    }
}

fn lang_label(l: Lang, lang: Option<Lang>) -> &'static str {
    match lang {
        None => t(l, "Language: as the system"),
        Some(Lang::En) => "English",
        Some(Lang::Fr) => "Français",
        Some(Lang::Es) => "Español",
    }
}

fn local_error(l: Lang, e: local::ReadError) -> ApiError {
    ApiError::Failed(match e {
        local::ReadError::NotInstalled => t(l, "No server was installed on this computer by the installer.").to_owned(),
        local::ReadError::NeedsAdministrator(_) => t(l, "Its settings file is for administrators only.").to_owned(),
        local::ReadError::Cancelled => {
            t(l, "The password was not given: nothing was read. Type the address and the token below instead.")
                .to_owned()
        }
        local::ReadError::Failed(why) => tf(l, "Its settings file could not be read: {}", &[&why]),
    })
}

/// What a failed call means for the person, and what they can do.
pub fn explain(l: Lang, e: &ApiError) -> String {
    let (what, detail) = match e {
        ApiError::Unreachable(d) => (
            t(
                l,
                "No server answers at this address. Is it running? Is the address right (with its port, e.g. localhost:8080)?",
            ),
            d,
        ),
        ApiError::NotNuage(d) => (t(l, "This address does not answer as a Nuage DB server."), d),
        ApiError::Unauthorized(d) => (
            t(
                l,
                "The server refused this token. The editor needs the management token (NUAGE_ADMIN_TOKEN), not an application's token.",
            ),
            d,
        ),
        ApiError::Forbidden(d) => (t(l, "The server refused:"), d),
        ApiError::Refused(d) => (t(l, "Not saved:"), d),
        ApiError::NotFound(d) => (t(l, "Not found:"), d),
        ApiError::Failed(d) => (t(l, "It failed:"), d),
    };
    if detail.is_empty() { what.to_owned() } else { format!("{what} ({detail})") }
}

fn notice(ui: &mut egui::Ui, n: &Notice) {
    let color = if n.ok { Color32::from_rgb(4, 150, 87) } else { ui.visuals().error_fg_color };
    egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, color)).inner_margin(10.0).show(ui, |ui| {
        ui.label(RichText::new(&n.text).color(color));
    });
}

fn session_page(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    ui.add_space(8.0);
    ui.heading(t(l, session.section.label()));
    if let Some(n) = &session.notice {
        notice(ui, n);
    }
    if session.busy() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(t(l, "Saving…"));
        });
    }
    ui.add_space(6.0);
    match session.section {
        Section::Overview => crate::screens::overview(ui, session_view(session), l),
        Section::Applications => applications(ui, session, l),
        Section::Memory => memory(ui, session, l),
        Section::Limits => limits(ui, session, l),
        Section::Startup => crate::screens::startup(ui, session.settings.as_ref(), l),
    }
}

/// What the overview reads of a session.
fn session_view(session: &Session) -> crate::screens::View<'_> {
    crate::screens::View {
        address: session.server.address(),
        settings: session.settings.as_ref(),
        apps: session.apps.as_deref(),
    }
}

/// A setting's field, with where its value comes from and the button that gives it back.
fn setting_field(ui: &mut egui::Ui, session: &mut Session, l: Lang, key: &str, label: &str, help: &str) {
    let ctx = ui.ctx().clone();
    let read_only = session.settings.as_ref().is_some_and(|s| s.read_only);
    let setting = session.settings.as_ref().and_then(|s| s.get(key)).cloned();
    ui.add_space(6.0);
    let name = ui.strong(label);
    let field = session.fields.entry(key.to_owned()).or_default();
    ui.add_enabled(!read_only, egui::TextEdit::singleline(field).desired_width(180.0)).labelled_by(name.id);
    ui.label(RichText::new(help).weak());
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
                session.act(&ctx, move |server| server.reset_setting(&key).map(Outcome::Settings));
            }
        }
    });
}

/// Sends the fields of `keys` that changed.
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
    session.act(ui.ctx(), move |server| {
        let list: Vec<(&str, Option<String>)> = changes.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        server.save_settings(&list).map(Outcome::Settings)
    });
}

fn memory(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    let Some(settings) = session.settings.clone() else {
        ui.spinner();
        return;
    };
    let m = &settings.memory;
    match (m.total_bytes, m.used_bytes) {
        (Some(total), Some(used)) => {
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
    ui.label(RichText::new(t(l, "Amounts are written auto, 512 MiB or 2 GiB (a number alone is in MiB).")).weak());
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
    if ui.add_enabled(!session.busy() && !settings.read_only, egui::Button::new(t(l, "Save the memory"))).clicked() {
        save_fields(ui, session, l, &["memory.total_mb", "memory.cache_mb", "memory.kv_mb"]);
    }

    ui.add_space(16.0);
    ui.heading(t(l, "Memory by application"));
    ui.label(t(
        l,
        "Each application shares the page caches of the server by use (auto), or has an amount set apart for it alone.",
    ));
    ui.add_space(6.0);
    let ctx = ui.ctx().clone();
    egui::Grid::new("memory-by-app").striped(true).num_columns(6).spacing([14.0, 6.0]).show(ui, |ui| {
        for h in ["Application", "Its cache holds", "may hold", "Page cache", "Key-value store", ""] {
            ui.strong(if h.is_empty() { "" } else { t(l, h) });
        }
        ui.end_row();
        for a in &settings.applications {
            ui.label(&a.name);
            ui.label(format::bytes(l, a.cache_bytes));
            ui.label(format::bytes(l, a.cache_capacity_bytes));
            let (cache, kv) =
                session.app_fields.entry(a.name.clone()).or_insert_with(|| ("auto".into(), "auto".into()));
            ui.name_inputs(format!("{} · {}", a.name, t(l, "Page cache")), |ui| {
                ui.add_enabled(!settings.read_only, egui::TextEdit::singleline(cache).desired_width(100.0))
            });
            ui.name_inputs(format!("{} · {}", a.name, t(l, "Key-value store")), |ui| {
                ui.add_enabled(!settings.read_only, egui::TextEdit::singleline(kv).desired_width(100.0))
            });
            let (cache, kv) = (cache.clone(), kv.clone());
            let label = format!("{} · {}", t(l, "Save"), a.name);
            if ui.add_enabled(!session.busy() && !settings.read_only, egui::Button::new(label)).clicked() {
                let name = a.name.clone();
                session.act(&ctx, move |server| server.set_memory(&name, Some(&cache), Some(&kv)).map(Outcome::Memory));
            }
            ui.end_row();
        }
    });
}

fn limits(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    let Some(settings) = session.settings.clone() else {
        ui.spinner();
        return;
    };
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
        t(l, "Each has a thread; the next ones are refused as PostgreSQL does."),
    );
    ui.add_space(8.0);
    if ui.add_enabled(!session.busy() && !settings.read_only, egui::Button::new(t(l, "Save the limits"))).clicked() {
        save_fields(ui, session, l, &["statement.memory_mb", "statement.seconds", "postgres.connections"]);
    }
    ui.add_space(16.0);
    ui.heading(t(l, "Visibility"));
    let before = session.public;
    ui.add_enabled(
        !settings.read_only && !session.busy(),
        egui::Checkbox::new(
            &mut session.public,
            t(l, "Public status: the dashboard shows a public subset without signing in"),
        ),
    );
    if session.public != before {
        let value = session.public.to_string();
        session.act(ui.ctx(), move |server| {
            server.save_settings(&[("status.public", Some(value))]).map(Outcome::Settings)
        });
    }
}

fn applications(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    let ctx = ui.ctx().clone();
    // A new application.
    egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
        ui.strong(t(l, "New application"));
        ui.label(
            RichText::new(t(
                l,
                "Each application has its own encrypted database and its own tokens: one per website or program.",
            ))
            .weak(),
        );
        egui::Grid::new("new-app").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            let label = ui.label(t(l, "Name"));
            ui.add(egui::TextEdit::singleline(&mut session.new_name).hint_text("shop").desired_width(240.0))
                .labelled_by(label.id);
            ui.end_row();
            let label = ui.label(t(l, "Description"));
            ui.add(
                egui::TextEdit::singleline(&mut session.new_description)
                    .hint_text(t(l, "what this application is"))
                    .desired_width(240.0),
            )
            .labelled_by(label.id);
            ui.end_row();
        });
        ui.label(RichText::new(t(l, "A name: lowercase letters, digits, - and _ (1 to 63).")).weak());
        let name = session.new_name.trim().to_owned();
        if ui
            .add_enabled(!session.busy() && !name.is_empty(), egui::Button::new(t(l, "Create the application")))
            .clicked()
        {
            let description = session.new_description.trim().to_owned();
            session.act(&ctx, move |server| {
                server.create_application(&name, &description).map(|(app, c)| Outcome::Created(app, c))
            });
        }
    });
    ui.add_space(10.0);
    let Some(apps) = session.apps.clone() else {
        ui.spinner();
        return;
    };
    for app in &apps {
        egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(&app.name);
                if app.first {
                    ui.label(RichText::new(t(l, "the server's own")).weak());
                }
            });
            if !app.description.is_empty() {
                ui.label(&app.description);
            }
            ui.label(
                RichText::new(tf(
                    l,
                    "Memory: page cache {}, key-value store {}.",
                    &[&format::amount(l, app.memory.cache_mb), &format::amount(l, app.memory.kv_mb)],
                ))
                .weak(),
            );
            ui.add_space(4.0);
            ui.strong(t(l, "Tokens"));
            if app.env_token {
                ui.label(RichText::new(t(l, "NUAGE_TOKEN (set on the server): read and write.")).weak());
            }
            if app.tokens.is_empty() && !app.env_token {
                ui.label(RichText::new(t(l, "No token: no program can connect yet.")).weak());
            }
            for token in &app.tokens {
                ui.horizontal(|ui| {
                    ui.monospace(format!("nuage_{}…", token.id));
                    ui.label(if token.read_only { t(l, "read only") } else { t(l, "read and write") });
                    ui.label(RichText::new(format::date(token.created_at)).weak());
                    if ui.add_enabled(!session.busy(), egui::Button::new(t(l, "Revoke")).small()).clicked() {
                        session.confirm = Some(Confirm { app: app.name.clone(), id: token.id.clone() });
                    }
                });
            }
            ui.horizontal(|ui| {
                for (read_only, label) in [(false, t(l, "New token (read and write)")), (true, t(l, "New read-only token"))] {
                    if ui.add_enabled(!session.busy(), egui::Button::new(label)).clicked() {
                        let name = app.name.clone();
                        session.act(&ctx, move |server| {
                            server.new_token(&name, read_only).map(|connection| Outcome::Token {
                                app: name,
                                read_only,
                                connection,
                            })
                        });
                    }
                }
            });
            ui.label(
                RichText::new(t(
                    l,
                    "A token in a web page is public: give pages a read-only token, and keep read-and-write tokens on servers.",
                ))
                .small()
                .weak(),
            );
        });
        ui.add_space(6.0);
    }
    ui.label(
        RichText::new(t(
            l,
            "Deleting an application (its data and its backups) is done in the console, behind a typed confirmation.",
        ))
        .small()
        .weak(),
    );
}

/// The windows above the page: a token shown once, a revocation to confirm.
fn dialogs(ctx: &egui::Context, session: &mut Session, l: Lang) {
    if let Some(reveal) = &session.reveal {
        let mut close = false;
        egui::Modal::new(egui::Id::unique("reveal")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.heading(tf(l, "The token of {}", &[&reveal.app]));
            ui.label(if reveal.read_only {
                t(l, "Read only: for the pages of a website.")
            } else {
                t(l, "Read and write: for a server or a program, never in a web page.")
            });
            ui.label(
                RichText::new(t(l, "Copy it now: it is not shown again (the server keeps only a fingerprint of it)."))
                    .color(ui.visuals().warn_fg_color),
            );
            ui.add_space(6.0);
            let mut lines = reveal.connection.env_lines();
            ui.name_inputs(t(l, "Connection lines"), |ui| {
                ui.add(egui::TextEdit::multiline(&mut lines).code_editor().desired_rows(2).desired_width(f32::INFINITY))
            });
            ui.horizontal(|ui| {
                if ui.button(t(l, "Copy the two lines")).clicked() {
                    ui.ctx().copy_text(reveal.connection.env_lines());
                }
                if ui.button(t(l, "Copy the token")).clicked() {
                    ui.ctx().copy_text(reveal.connection.token.clone());
                }
            });
            ui.label(
                RichText::new(t(l, "Give these two lines to the program, as its settings (environment variables)."))
                    .weak(),
            );
            ui.add_space(6.0);
            if ui.button(t(l, "I have copied it: close")).clicked() {
                close = true;
            }
        });
        if close {
            session.reveal = None;
        }
    }
    if let Some(confirm) = &session.confirm {
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(480.0);
            ui.heading(t(l, "Revoke this token?"));
            ui.label(tf(
                l,
                "nuage_{}… of {} will open nothing any more: the programs that use it lose access at once. It cannot be undone (make a new token instead).",
                &[&confirm.id, &confirm.app],
            ));
            ui.horizontal(|ui| {
                if ui.button(t(l, "Revoke")).clicked() {
                    answer = Some(true);
                }
                if ui.button(t(l, "Cancel")).clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(yes) = answer
            && let Some(Confirm { app, id }) = session.confirm.take()
            && yes
        {
            session.act(ctx, move |server| server.revoke_token(&app, &id).map(|()| Outcome::Revoked { app, id }));
        }
    }
}
