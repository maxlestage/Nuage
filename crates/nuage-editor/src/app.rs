//! The window: connecting to a server, then its applications, its memory,
//! its limits and what it was started with.
//!
//! The window only draws and records clicks: the network, the password
//! store and the files of this computer are reached by jobs (see
//! [`crate::jobs`]), whose answers are taken at the next frame.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use zeroize::Zeroizing;

use crate::address::{self, AddressError};
use crate::api::{self, ApiError, Application, Caps, Connected, Connection, Server, Settings, Transport};
use crate::clip::{ArboardSink, Copied, CopySink};
use crate::format;
use crate::i18n::{Lang, t, tf};
use crate::jobs::{Calls, Job, take};
use crate::local;
use crate::secret::Secret;
use crate::vault::{self, KeyringVault, Vault};

/// What eframe keeps between two launches. No field can hold a secret: a
/// kept token is in the system's password store, and here only `token_kept`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The language chosen (`None`: the system's).
    pub lang: Option<Lang>,
    pub theme: Theme,
    /// The servers connected to, the last one first.
    pub servers: Vec<SavedServer>,
}

/// A server connected to before.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SavedServer {
    /// Its address, normalized.
    pub address: String,
    /// Its management token is in the password store.
    pub token_kept: bool,
}

/// Light, dark, or as the system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// What the editor works with: the password store, the clipboard, the
/// limits of the calls. The program takes the system's; the tests, theirs.
pub struct Setup {
    pub vault: Arc<dyn Vault>,
    pub clipboard: Box<dyn CopySink>,
    pub transport: Transport,
    /// The editor's folder (eframe's storage), where an early build left a `tokens.json`.
    pub storage_dir: Option<PathBuf>,
    /// Looks for a server installed on this computer.
    pub detect_local: bool,
    /// An address and a management token to connect to at once (`NUAGE_URL`
    /// and `NUAGE_ADMIN_TOKEN` of a shortcut or a script).
    pub connect_to: Option<(String, Secret)>,
}

impl Setup {
    /// The system's store and clipboard; `storage_dir`: eframe's folder.
    pub fn system(storage_dir: Option<PathBuf>) -> Setup {
        let var = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let connect_to = match (var("NUAGE_URL"), var("NUAGE_ADMIN_TOKEN")) {
            (Some(url), Some(token)) => Some((url, Secret::new(token))),
            _ => None,
        };
        Setup {
            vault: Arc::new(KeyringVault),
            clipboard: Box::new(ArboardSink::default()),
            transport: Transport::default(),
            storage_dir,
            detect_local: true,
            connect_to,
        }
    }

    /// `vault` and `clipboard`, nothing looked for on this computer, nothing
    /// read from the environment.
    pub fn with(vault: Arc<dyn Vault>, clipboard: Box<dyn CopySink>) -> Setup {
        Setup {
            vault,
            clipboard,
            transport: Transport::default(),
            storage_dir: None,
            detect_local: false,
            connect_to: None,
        }
    }
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

/// Where the token of a connection comes from.
enum Given {
    /// The field.
    Typed,
    /// The password store.
    Kept,
    /// A settings file of this computer, or the environment.
    Secret(Secret),
}

/// What a connection comes back with: the server, and how keeping its
/// token went (`None`: it was not asked).
type Connecting = (Connected, Option<Result<(), String>>);

/// The id of the token's field: its state (and its undo copies) is dropped
/// once connected.
fn token_field() -> egui::Id {
    egui::Id::unique("nuage-editor-management-token")
}

/// What the connection form holds.
#[derive(Default)]
struct ConnectForm {
    address: String,
    /// Typed, wiped when connected.
    token: Zeroizing<String>,
    show_token: bool,
    remember: bool,
    /// Use the token kept for this address (the person did not ask to type another).
    use_kept: bool,
    /// The address whose plain http was accepted ("Connect anyway"), for this attempt only.
    cleartext_ok: Option<String>,
    /// The public address (`SITE_URL`) of the server of this computer: its address, and it.
    public: Option<(String, String)>,
    job: Option<Job<Connecting>>,
    /// The connection running uses the kept token.
    with_kept: bool,
    notice: Option<Notice>,
    /// After a redirect to the same host over https: that address.
    upgrade: Option<String>,
    /// Reading the settings file of this computer: what it gave, and whether
    /// the system can ask for the administrator's password (looked up there too).
    local_job: Option<Job<(Result<local::Found, local::ReadError>, bool)>>,
    /// The settings file of a server of this computer that needs the
    /// administrator's password, and whether the system can ask for it.
    needs_admin: Option<(PathBuf, bool)>,
}

/// What a change sent to the server comes back with.
enum Outcome {
    Settings(Settings),
    Created(Application, Connection, Option<String>),
    Token { app: String, read_only: bool, connection: Connection, lan: Option<String> },
    Revoked { app: String, id: String },
    Memory(Application),
}

/// A token shown once, in a window of its own, until its owner closes it.
struct Reveal {
    app: String,
    read_only: bool,
    connection: Connection,
    /// This computer's address on its network, when `NUAGE_URL` reaches this computer only.
    lan: Option<String>,
    copied: Option<Copied>,
}

/// The id of the field that shows the connection lines (its state is dropped on close).
fn lines_field() -> egui::Id {
    egui::Id::unique("nuage-editor-connection-lines")
}

/// A token about to be revoked: its owner confirms.
struct Confirm {
    app: String,
    id: String,
}

/// A server the editor is connected to.
struct Session {
    server: Server,
    calls: Calls,
    section: Section,
    settings: Option<Settings>,
    /// Why the settings could not be read (a server that has them).
    settings_error: Option<ApiError>,
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
    fn new(connected: Connected, lang: Lang, calls: Calls) -> Session {
        let Connected { server, apps, settings, settings_error } = connected;
        let mut session = Session {
            server,
            calls,
            section: Section::Overview,
            settings: None,
            settings_error,
            apps: Some(apps),
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
        if let Some(settings) = settings {
            session.take_settings(settings, lang);
        }
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
        self.settings_error = None;
    }

    fn reload(&mut self, ctx: &egui::Context) {
        if self.load_settings.is_none() && self.server.caps().settings {
            let server = self.server.clone();
            self.load_settings = Some(Job::start(ctx, &self.calls, move || server.settings()));
        }
        if self.load_apps.is_none() {
            let server = self.server.clone();
            self.load_apps = Some(Job::start(ctx, &self.calls, move || server.applications()));
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
        self.action = Some(Job::start(ctx, &self.calls, move || work(server)));
    }
}

/// This computer's address on its network, for a `NUAGE_URL` that reaches
/// this computer only (run in the job that made the token).
fn lan_for(connection: &Connection) -> Option<String> {
    connection.is_this_computer_only().then(address::lan_ip).flatten().map(|ip| ip.to_string())
}

/// The editor's window.
pub struct Editor {
    pub prefs: Prefs,
    vault: Arc<dyn Vault>,
    /// Whether tokens can be kept (`None`: still asking the system).
    vault_status: Option<Result<(), String>>,
    vault_job: Option<Job<Result<(), String>>>,
    /// A kept token being forgotten: its address, and how it went.
    forget_job: Option<Job<(String, Result<(), String>)>>,
    old_file_job: Option<Job<Result<bool, String>>>,
    clipboard: Box<dyn CopySink>,
    transport: Transport,
    calls: Calls,
    storage_dir: Option<PathBuf>,
    detect_local: bool,
    /// The settings file of a server of this computer (`None`: still looking).
    installed: Option<Option<PathBuf>>,
    installed_job: Option<Job<Option<PathBuf>>>,
    /// The jobs of the first frame were started.
    started: bool,
    connect_to: Option<(String, Secret)>,
    connect: ConnectForm,
    session: Option<Session>,
    /// The theme the window was last given.
    theme_applied: Option<Theme>,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // What it shows, never a token.
        f.debug_struct("Editor")
            .field("prefs", &self.prefs)
            .field("address", &self.connect.address)
            .field("server", &self.session.as_ref().map(|s| &s.server))
            .finish_non_exhaustive()
    }
}

impl Editor {
    /// The editor with what was remembered (`prefs`), working with `setup`.
    /// Nothing is read or called before the first frame.
    pub fn new(prefs: Prefs, setup: Setup) -> Editor {
        let mut connect = ConnectForm::default();
        if let Some(last) = prefs.servers.first() {
            connect.address = last.address.clone();
            connect.use_kept = last.token_kept;
        }
        Editor {
            prefs,
            vault: setup.vault,
            vault_status: None,
            vault_job: None,
            forget_job: None,
            old_file_job: None,
            clipboard: setup.clipboard,
            transport: setup.transport,
            calls: Calls::default(),
            storage_dir: setup.storage_dir,
            detect_local: setup.detect_local,
            installed: None,
            installed_job: None,
            started: false,
            connect_to: setup.connect_to,
            connect,
            session: None,
            theme_applied: None,
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

    /// The address typed in the connection form.
    pub fn typed_address(&self) -> &str {
        &self.connect.address
    }

    /// The address of the server connected to.
    pub fn connected_to(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.server.address())
    }

    /// What the server connected to has.
    pub fn caps(&self) -> Option<&Caps> {
        self.session.as_ref().map(|s| s.server.caps())
    }

    /// Whether tokens can be kept on this computer (`None`: still asking).
    pub fn can_keep_tokens(&self) -> Option<&Result<(), String>> {
        self.vault_status.as_ref()
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
        if !std::mem::replace(&mut self.started, true) {
            self.start(&ctx);
        }
        self.poll(&ctx);
        if self.waiting() {
            // The seconds waited, and "Stop waiting", show while nothing else repaints.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
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
            dialogs(&ctx, session, l, self.clipboard.as_mut());
        }
    }

    /// The jobs of the first frame: is there a password store, a server on
    /// this computer, a file an early build left.
    fn start(&mut self, ctx: &egui::Context) {
        let vault = self.vault.clone();
        self.vault_job = Some(Job::start(ctx, &self.calls, move || Ok(vault.status())));
        if let Some(dir) = self.storage_dir.clone() {
            self.old_file_job = Some(Job::start(ctx, &self.calls, move || Ok(vault::delete_old_tokens_file(&dir))));
        }
        if self.detect_local {
            self.installed_job = Some(Job::start(ctx, &self.calls, || Ok(local::installed())));
        } else {
            self.installed = Some(None);
        }
        if let Some((address, token)) = self.connect_to.take() {
            self.connect.address = address;
            self.connect.use_kept = false;
            self.start_connect(ctx, Given::Secret(token));
        }
    }

    fn waiting(&self) -> bool {
        self.connect.job.is_some()
            || self.connect.local_job.is_some()
            || self.vault_job.is_some()
            || self.session.as_ref().is_some_and(|s| s.busy() || s.load_apps.is_some() || s.load_settings.is_some())
    }

    /// The saved server at `address`.
    fn saved(&self, address: &str) -> Option<&SavedServer> {
        self.prefs.servers.iter().find(|s| s.address == address)
    }

    /// The address typed, normalized.
    fn typed_base(&self) -> Option<String> {
        address::normalize(&self.connect.address).ok().map(|a| a.base())
    }

    /// The token of the address typed is kept, and will be used.
    fn kept_here(&self) -> bool {
        self.connect.use_kept && self.typed_base().is_some_and(|b| self.saved(&b).is_some_and(|s| s.token_kept))
    }

    /// Takes the answers of the calls that came back.
    fn poll(&mut self, ctx: &egui::Context) {
        let l = self.lang();
        if let Some(answer) = take(&mut self.vault_job) {
            self.vault_status = Some(answer.unwrap_or_else(|e| Err(e.explain(l))));
        }
        if let Some(answer) = take(&mut self.old_file_job) {
            match answer {
                Ok(Ok(true)) => {
                    self.connect.notice = Some(Notice::ok(t(
                        l,
                        "An early version of the editor kept tokens in a file, in clear: that file is now deleted. Type your token again; tick \"Remember on this computer\" to keep it in your system's password store.",
                    )))
                }
                Ok(Ok(false)) => {}
                Ok(Err(why)) | Err(ApiError::Bug(why)) => {
                    self.connect.notice = Some(Notice::bad(tf(
                        l,
                        "A file of an early version of the editor, with tokens in clear, could not be deleted ({}): delete it yourself.",
                        &[&why],
                    )))
                }
                Err(e) => self.connect.notice = Some(Notice::bad(e.explain(l))),
            }
        }
        if let Some(answer) = take(&mut self.installed_job) {
            self.installed = Some(answer.unwrap_or(None));
        }
        if let Some(answer) = take(&mut self.forget_job) {
            match answer {
                Ok((address, Ok(()))) => {
                    for s in self.prefs.servers.iter_mut().filter(|s| s.address == address) {
                        s.token_kept = false;
                    }
                    self.connect.notice =
                        Some(Notice::ok(t(l, "Token forgotten: it is no longer kept on this computer.")));
                }
                Ok((_, Err(why))) => {
                    self.connect.notice =
                        Some(Notice::bad(tf(l, "The password store did not forget the token: {}", &[&why])))
                }
                Err(e) => self.connect.notice = Some(Notice::bad(e.explain(l))),
            }
        }
        if let Some(answer) = take(&mut self.connect.job) {
            self.connected(ctx, answer);
        }
        if let Some(answer) = take(&mut self.connect.local_job) {
            match answer {
                Ok((Ok(found), _)) => self.use_local(ctx, found),
                Ok((Err(local::ReadError::NeedsAdministrator(file)), can_ask)) => {
                    self.connect.needs_admin = Some((file, can_ask))
                }
                Ok((Err(e), _)) => self.connect.notice = Some(Notice::bad(local_error(l, e))),
                Err(e) => self.connect.notice = Some(Notice::bad(e.explain(l))),
            }
        }
        let Some(session) = self.session.as_mut() else { return };
        if let Some(answer) = take(&mut session.load_settings) {
            match answer {
                Ok(settings) => session.take_settings(settings, l),
                Err(e) => {
                    session.notice = Some(Notice::bad(e.explain(l)));
                    session.settings_error = Some(e);
                }
            }
        }
        if let Some(answer) = take(&mut session.load_apps) {
            match answer {
                Ok(apps) => session.apps = Some(apps),
                Err(e) => session.notice = Some(Notice::bad(e.explain(l))),
            }
        }
        if let Some(answer) = take(&mut session.action) {
            match answer {
                Ok(outcome) => {
                    let notice = match outcome {
                        Outcome::Settings(settings) => {
                            let warning = settings.warning.clone();
                            session.take_settings(settings, l);
                            match warning {
                                Some(w) => Notice::bad(format!("{} {w}", t(l, "Saved and in force, but:"))),
                                None => Notice::ok(t(l, "Saved: in force now, with no restart.")),
                            }
                        }
                        Outcome::Created(app, connection, lan) => {
                            session.new_name.clear();
                            session.new_description.clear();
                            let notice =
                                tf(l, "Application {} created, with its own encrypted database.", &[&app.name]);
                            session.reveal =
                                Some(Reveal { app: app.name, read_only: false, connection, lan, copied: None });
                            Notice::ok(notice)
                        }
                        Outcome::Token { app, read_only, connection, lan } => {
                            let notice = tf(l, "New token for {}.", &[&app]);
                            session.reveal = Some(Reveal { app, read_only, connection, lan, copied: None });
                            Notice::ok(notice)
                        }
                        Outcome::Revoked { app, id } => {
                            Notice::ok(tf(l, "Token {}… of {} revoked: it opens nothing any more.", &[&id, &app]))
                        }
                        Outcome::Memory(app) => Notice::ok(tf(l, "Memory of {} saved: in force now.", &[&app.name])),
                    };
                    session.notice = Some(notice);
                    session.reload(ctx);
                }
                Err(ApiError::Timeout { .. }) => {
                    // A write without an answer may have been made: what is in force is read again.
                    session.notice = Some(Notice::bad(t(
                        l,
                        "The server did not answer in time: the change may or may not have been made. The page is read again to show what is in force.",
                    )));
                    session.reload(ctx);
                }
                Err(e) => session.notice = Some(Notice::bad(e.explain(l))),
            }
        }
    }

    /// A connection came back.
    fn connected(&mut self, ctx: &egui::Context, answer: Result<Connecting, ApiError>) {
        let l = self.lang();
        let with_kept = std::mem::take(&mut self.connect.with_kept);
        match answer {
            Ok((connected, kept)) => {
                let address = connected.server.address().to_owned();
                let was_kept = self.saved(&address).is_some_and(|s| s.token_kept);
                let mut notice = None;
                let token_kept = match kept {
                    Some(Ok(())) => true,
                    Some(Err(why)) => {
                        notice = Some(Notice::bad(tf(
                            l,
                            "Connected, but the token could not be kept in {} ({}): it is used until you close the editor.",
                            &[&self.vault.store().place(l), &why],
                        )));
                        was_kept
                    }
                    None => was_kept || with_kept,
                };
                self.prefs.servers.retain(|s| s.address != address);
                self.prefs.servers.insert(0, SavedServer { address: address.clone(), token_kept });
                self.prefs.servers.truncate(8);
                // The typed token is wiped (Zeroizing), and egui's state of its field with it.
                self.connect.token = Zeroizing::new(String::new());
                ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(token_field()));
                self.connect.address = address;
                self.connect.show_token = false;
                self.connect.cleartext_ok = None;
                self.connect.use_kept = token_kept;
                self.connect.notice = None;
                let mut session = Session::new(connected, l, self.calls.clone());
                session.notice = notice;
                self.session = Some(session);
            }
            Err(ApiError::TokenRefused) if with_kept => {
                // Kept until a new one works: never deleted without a word.
                self.connect.use_kept = false;
                self.connect.remember = true;
                self.connect.notice = Some(Notice::bad(t(
                    l,
                    "The kept token no longer works: it was changed on the server. Type the new one; it replaces the kept one once it works.",
                )));
            }
            Err(e @ (ApiError::NotKept | ApiError::Keystore(_))) => {
                // The field comes back; a token the store no longer has is no longer counted as kept.
                if e == ApiError::NotKept
                    && let Some(base) = self.typed_base()
                {
                    for s in self.prefs.servers.iter_mut().filter(|s| s.address == base) {
                        s.token_kept = false;
                    }
                }
                self.connect.use_kept = false;
                self.connect.notice = Some(Notice::bad(e.explain(l)));
            }
            Err(e) => {
                if let ApiError::Redirect { upgrade: Some(to), .. } = &e {
                    self.connect.upgrade = Some(to.clone());
                }
                self.connect.notice = Some(Notice::bad(e.explain(l)));
            }
        }
    }

    /// The settings file of the server of this computer was read.
    fn use_local(&mut self, ctx: &egui::Context, found: local::Found) {
        let l = self.lang();
        let local::Found { address, site_url, token, .. } = found;
        self.connect.needs_admin = None;
        self.connect.address = address.clone();
        self.connect.use_kept = false;
        self.connect.public = site_url.map(|site| (address, site));
        match token {
            Some(token) => self.start_connect(ctx, Given::Secret(token)),
            None => {
                self.connect.notice = Some(Notice::bad(t(
                    l,
                    "The settings file of this computer has no management token (NUAGE_ADMIN_TOKEN): run the installer again, it adds one.",
                )))
            }
        }
    }

    fn start_connect(&mut self, ctx: &egui::Context, given: Given) {
        let l = self.lang();
        let address = match address::normalize(&self.connect.address) {
            Ok(a) => a,
            Err(e) => {
                self.connect.notice = Some(Notice::bad(address_error(l, &self.connect.address, &e)));
                return;
            }
        };
        let base = address.base();
        if address.is_cleartext_public() && self.connect.cleartext_ok.as_deref() != Some(base.as_str()) {
            self.connect.notice = Some(Notice::bad(t(
                l,
                "Not encrypted: the token would cross the Internet in clear. Use the server's https:// address.",
            )));
            return;
        }
        let token = match given {
            Given::Secret(token) => Some(token),
            Given::Kept => None,
            Given::Typed => match api::typed_token(&self.connect.token) {
                Ok(token) => Some(token),
                Err(e) => {
                    self.connect.notice = Some(Notice::bad(e.explain(l)));
                    return;
                }
            },
        };
        let remember = self.connect.remember && token.is_some() && matches!(self.vault_status, Some(Ok(())));
        let public = self.connect.public.as_ref().filter(|(a, _)| *a == base).map(|(_, site)| site.clone());
        let (vault, transport) = (self.vault.clone(), self.transport.clone());
        self.connect.with_kept = token.is_none();
        self.connect.notice = None;
        self.connect.upgrade = None;
        self.connect.job = Some(Job::start(ctx, &self.calls, move || {
            let token = match token {
                Some(token) => token,
                None => vault.get(&base).map_err(ApiError::Keystore)?.ok_or(ApiError::NotKept)?,
            };
            let connected = Server::new(&base, token, l, &transport)?.with_public(public).connect()?;
            let kept = remember.then(|| vault.set(connected.server.address(), connected.server.token()));
            Ok((connected, kept))
        }));
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, l: Lang) {
        ui.horizontal_wrapped(|ui| {
            ui.heading(RichText::new("Nuage DB").strong());
            ui.label(RichText::new(t(l, "Editor")).weak());
            ui.separator();
            let connected = self.session.as_ref().map(|s| (s.server.address().to_owned(), s.busy()));
            match &connected {
                Some((address, _)) => {
                    dot(ui, Some(Color32::from_rgb(4, 150, 87)));
                    ui.label(tf(l, "Connected to {}", &[address]));
                    if address::normalize(address).is_ok_and(|a| a.is_cleartext_public()) {
                        ui.label(RichText::new(t(l, "not encrypted")).color(ui.visuals().error_fg_color));
                    }
                }
                None => {
                    dot(ui, None);
                    ui.label(t(l, "Not connected"));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Not while a change is on its way: its answer would be lost.
                if let Some((_, busy)) = connected
                    && ui.add_enabled(!busy, egui::Button::new(t(l, "Disconnect"))).clicked()
                {
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
        self.this_computer(ui, &ctx, l, waiting);
        ui.add_space(10.0);
        self.address_and_token(ui, &ctx, l, waiting);
        if let Some(n) = &self.connect.notice {
            notice(ui, n);
        }
        if let Some(to) = self.connect.upgrade.clone()
            && ui.button(tf(l, "Use {}", &[&to])).clicked()
        {
            self.connect.address = to;
            self.connect.upgrade = None;
            self.connect.notice = None;
        }
        self.servers_used(ui, &ctx, l);
    }

    /// A server of this computer, installed by the installer.
    fn this_computer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang, waiting: bool) {
        egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
            ui.strong(t(l, "On this computer"));
            match self.installed.clone() {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t(l, "Looking for a Nuage DB server installed here…"));
                    });
                }
                Some(Some(file)) => {
                    ui.label(tf(
                        l,
                        "A Nuage DB server is installed here (its settings are in {}).",
                        &[&file.display()],
                    ));
                    if let Some((needs, can_ask)) = self.connect.needs_admin.clone() {
                        if can_ask {
                            ui.label(t(
                                l,
                                "Its settings file is protected (it holds the encryption key): your system asks for your password to read its address and its management token, and nothing else.",
                            ));
                            if ui.add_enabled(!waiting, egui::Button::new(t(l, "Enter my password"))).clicked() {
                                self.connect.local_job = Some(Job::start(ctx, &self.calls, move || {
                                    Ok((local::read_as_administrator(&needs), true))
                                }));
                            }
                        } else {
                            ui.label(t(
                                l,
                                "Its settings file is protected (it holds the encryption key). Close the editor, then open it again as administrator (right-click: Run as administrator) to read it once, with \"Remember on this computer\" ticked.",
                            ));
                        }
                    } else if ui.add_enabled(!waiting, egui::Button::new(t(l, "Use this server"))).clicked() {
                        self.connect.local_job = Some(Job::start(ctx, &self.calls, || {
                            let read = local::read();
                            // Looked up here: it searches the PATH.
                            let can_ask = matches!(read, Err(local::ReadError::NeedsAdministrator(_)))
                                && local::can_ask_administrator();
                            Ok((read, can_ask))
                        }));
                    }
                }
                Some(None) => {
                    ui.label(t(
                        l,
                        "No server was installed on this computer by the installer. If yours is elsewhere (online, another computer), type its address below.",
                    ));
                }
            }
        });
    }

    /// Any server: its address and its management token.
    fn address_and_token(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang, waiting: bool) {
        let kept_here = self.kept_here();
        egui::Frame::group(ui.style()).inner_margin(12.0).show(ui, |ui| {
            ui.strong(t(l, "Address and management token"));
            let mut cleartext = None;
            egui::Grid::new("connect").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                let label = ui.label(t(l, "Address of the server"));
                ui.vertical(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.connect.address)
                            .hint_text("https://my-nuage.herokuapp.com  ·  localhost:8080")
                            .desired_width(380.0),
                    )
                    .labelled_by(label.id);
                    // What will be called, before anything is.
                    match address::normalize(&self.connect.address) {
                        Ok(a) => {
                            let mut line = tf(l, "Will connect to {}", &[&a.base()]);
                            if let Some(dropped) = &a.dropped {
                                line = format!("{line} {}", tf(l, "({} dropped)", &[dropped]));
                            }
                            ui.label(RichText::new(line).weak());
                            if a.is_cleartext_public() {
                                cleartext = Some(a.base());
                            }
                        }
                        Err(AddressError::Empty) => {}
                        Err(e) => {
                            ui.label(
                                RichText::new(address_error(l, &self.connect.address, &e))
                                    .color(ui.visuals().warn_fg_color),
                            );
                        }
                    }
                });
                ui.end_row();
                let label = ui.label(t(l, "Management token"));
                if kept_here {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(tf(l, "Token kept in {}.", &[&self.vault.store().place(l)]));
                        if let Some(base) = self.typed_base()
                            && ui.add_enabled(self.forget_job.is_none(), egui::Button::new(t(l, "Forget this token"))).clicked()
                        {
                            self.forget(ctx, base);
                        }
                        if ui.button(t(l, "Type another token")).clicked() {
                            self.connect.use_kept = false;
                        }
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut *self.connect.token)
                                .id(token_field())
                                .password(!self.connect.show_token)
                                .hint_text("NUAGE_ADMIN_TOKEN")
                                .desired_width(300.0),
                        )
                        .labelled_by(label.id);
                        ui.checkbox(&mut self.connect.show_token, t(l, "Show"));
                    });
                }
                ui.end_row();
            });
            if !kept_here {
                ui.label(
                    RichText::new(t(
                        l,
                        "The management token is NUAGE_ADMIN_TOKEN: in the settings file the installer wrote, or in your host's variables (Heroku: Settings, Config Vars). It is not an application's token.",
                    ))
                    .weak(),
                );
                self.remember_box(ui, l);
            }
            if let Some(base) = &cleartext {
                ui.label(
                    RichText::new(t(
                        l,
                        "Not encrypted: the token would cross the Internet in clear. Use the server's https:// address.",
                    ))
                    .color(ui.visuals().error_fg_color),
                );
                let mut anyway = self.connect.cleartext_ok.as_deref() == Some(base.as_str());
                if ui.checkbox(&mut anyway, t(l, "Connect anyway")).changed() {
                    self.connect.cleartext_ok = anyway.then(|| base.clone());
                }
            }
            let consented = cleartext.is_none() || self.connect.cleartext_ok == cleartext;
            ui.horizontal(|ui| {
                if ui.add_enabled(!waiting && consented, egui::Button::new(t(l, "Connect"))).clicked() {
                    self.start_connect(ctx, if kept_here { Given::Kept } else { Given::Typed });
                }
                if let Some(job) = &self.connect.job {
                    let elapsed = job.elapsed();
                    ui.spinner();
                    ui.label(tf(l, "Checking… ({} s)", &[&elapsed.as_secs()]));
                    if elapsed >= Duration::from_secs(3) && ui.button(t(l, "Stop waiting")).clicked() {
                        // The call ends by itself; its answer is dropped.
                        self.connect.job = None;
                        self.connect.with_kept = false;
                    }
                } else if waiting {
                    ui.spinner();
                }
            });
            if self.connect.job.as_ref().is_some_and(|j| j.elapsed() >= Duration::from_secs(8)) {
                ui.label(
                    RichText::new(t(l, "A server that was asleep (Heroku Eco) can take half a minute to wake.")).weak(),
                );
            }
        });
    }

    /// "Remember on this computer", when the system has a store the editor can use.
    fn remember_box(&mut self, ui: &mut egui::Ui, l: Lang) {
        let store = self.vault.store().place(l);
        match &self.vault_status {
            Some(Ok(())) => {
                ui.checkbox(&mut self.connect.remember, t(l, "Remember on this computer"));
                if self.connect.remember {
                    ui.label(RichText::new(tf(l, "The token goes in {}.", &[&store])).weak());
                }
                ui.label(
                    RichText::new(t(
                        l,
                        "This token can make tokens for every application: remember it only on your own computer.",
                    ))
                    .weak(),
                );
            }
            Some(Err(why)) => {
                let mut no = false;
                ui.add_enabled(false, egui::Checkbox::new(&mut no, t(l, "Remember on this computer")));
                ui.label(
                    RichText::new(tf(
                        l,
                        "Your system has no password store the editor can use ({}): the token is kept until you close the editor.",
                        &[why],
                    ))
                    .weak(),
                );
            }
            None => {
                let mut no = false;
                ui.add_enabled(false, egui::Checkbox::new(&mut no, t(l, "Remember on this computer")));
                let waited = self.vault_job.as_ref().is_some_and(|j| j.elapsed() >= Duration::from_secs(2));
                ui.label(
                    RichText::new(if waited {
                        t(l, "Waiting for your keyring…")
                    } else {
                        t(l, "Looking for this system's password store…")
                    })
                    .weak(),
                );
            }
        }
    }

    /// Forgets the token kept for `address`, in the password store.
    fn forget(&mut self, ctx: &egui::Context, address: String) {
        let vault = self.vault.clone();
        self.forget_job = Some(Job::start(ctx, &self.calls, move || {
            let done = vault.delete(&address);
            Ok((address, done))
        }));
    }

    /// The servers connected to before.
    fn servers_used(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, l: Lang) {
        if self.prefs.servers.is_empty() {
            return;
        }
        ui.add_space(12.0);
        ui.strong(t(l, "Servers you used"));
        let mut chosen = None;
        let mut forget = None;
        for saved in &self.prefs.servers {
            ui.horizontal(|ui| {
                if ui.link(&saved.address).clicked() {
                    chosen = Some(saved.clone());
                }
                if saved.token_kept {
                    ui.label(RichText::new(t(l, "token kept")).weak());
                }
                if ui.add_enabled(self.forget_job.is_none(), egui::Button::new(t(l, "Forget")).small()).clicked() {
                    forget = Some(saved.clone());
                }
            });
        }
        if let Some(saved) = chosen {
            self.connect.address = saved.address;
            self.connect.use_kept = saved.token_kept;
            self.connect.token = Zeroizing::new(String::new());
            self.connect.cleartext_ok = None;
            self.connect.upgrade = None;
            self.connect.notice = None;
        }
        if let Some(saved) = forget {
            self.prefs.servers.retain(|s| s.address != saved.address);
            if saved.token_kept {
                self.forget(ctx, saved.address);
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

/// Why `typed` is not an address, said before anything is sent.
fn address_error(l: Lang, typed: &str, e: &AddressError) -> String {
    match e {
        AddressError::Empty => t(l, "Type the address of the server first.").to_owned(),
        AddressError::Scheme(scheme) => tf(l, "The editor speaks http:// and https:// only, not {}://.", &[scheme]),
        AddressError::Host(_) => ApiError::BadAddress(typed.trim().to_owned()).explain(l),
        AddressError::Port(port) => tf(l, "{} is not a port: a port is a number from 1 to 65535.", &[port]),
    }
}

fn local_error(l: Lang, e: local::ReadError) -> String {
    match e {
        local::ReadError::NotInstalled => t(l, "No server was installed on this computer by the installer.").to_owned(),
        local::ReadError::NeedsAdministrator(_) => t(l, "Its settings file is for administrators only.").to_owned(),
        local::ReadError::Cancelled => {
            t(l, "The password was not given: nothing was read. Type the address and the token below instead.")
                .to_owned()
        }
        local::ReadError::Failed(why) => tf(l, "Its settings file could not be read: {}", &[&why]),
    }
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
    if let Some(action) = &session.action {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(tf(
                l,
                "Saving… ({} s; the editor waits up to {} s)",
                &[&action.elapsed().as_secs(), &session.server.timeout().as_secs()],
            ));
        });
    }
    ui.add_space(6.0);
    match session.section {
        Section::Overview => crate::screens::overview(ui, session_view(session), l),
        Section::Applications => applications(ui, session, l),
        Section::Memory => memory(ui, session, l),
        Section::Limits => limits(ui, session, l),
        Section::Startup => {
            if let Some(settings) = settings_or_why(ui, session, l) {
                crate::screens::startup(ui, Some(&settings), l);
            }
        }
    }
}

/// The settings, or what the page says instead: the server has none (an
/// older version), they failed, or they are on their way.
fn settings_or_why(ui: &mut egui::Ui, session: &Session, l: Lang) -> Option<Settings> {
    if !session.server.caps().settings {
        ui.label(RichText::new(t(l, crate::screens::NO_SETTINGS)).color(ui.visuals().warn_fg_color));
        return None;
    }
    match (&session.settings, &session.settings_error) {
        (Some(settings), _) => Some(settings.clone()),
        (None, Some(e)) => {
            ui.label(RichText::new(e.explain(l)).color(ui.visuals().error_fg_color));
            None
        }
        (None, None) => {
            ui.spinner();
            None
        }
    }
}

/// What the overview reads of a session.
fn session_view(session: &Session) -> crate::screens::View<'_> {
    crate::screens::View {
        address: session.server.address(),
        caps: session.server.caps(),
        settings: session.settings.as_ref(),
        settings_error: session.settings_error.as_ref(),
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
    let Some(settings) = settings_or_why(ui, session, l) else { return };
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
    let Some(settings) = settings_or_why(ui, session, l) else { return };
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
                let (app, connection) = server.create_application(&name, &description)?;
                let lan = lan_for(&connection);
                Ok(Outcome::Created(app, connection, lan))
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
                            let connection = server.new_token(&name, read_only)?;
                            let lan = lan_for(&connection);
                            Ok(Outcome::Token { app: name, read_only, connection, lan })
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
fn dialogs(ctx: &egui::Context, session: &mut Session, l: Lang, clipboard: &mut dyn CopySink) {
    if let Some(reveal) = &mut session.reveal {
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
                ui.add(
                    egui::TextEdit::multiline(&mut *lines)
                        .id(lines_field())
                        .code_editor()
                        .desired_rows(2)
                        .desired_width(f32::INFINITY),
                )
            });
            ui.horizontal(|ui| {
                if ui.button(t(l, "Copy the two lines")).clicked() {
                    reveal.copied = Some(clipboard.copy_secret(ui.ctx(), &reveal.connection.env_lines()));
                }
                if ui.button(t(l, "Copy the token")).clicked() {
                    reveal.copied = Some(clipboard.copy_secret(ui.ctx(), reveal.connection.token.expose()));
                }
            });
            match &reveal.copied {
                Some(Copied::Private) => {
                    ui.label(RichText::new(t(l, COPIED)).weak());
                }
                Some(Copied::Plain(why)) => {
                    ui.label(
                        RichText::new(tf(
                            l,
                            "Copied, but the clipboard history could not be asked to skip it ({}): it may keep it.",
                            &[why],
                        ))
                        .color(ui.visuals().warn_fg_color),
                    );
                }
                None => {}
            }
            ui.label(
                RichText::new(t(l, "Give these two lines to the program, as its settings (environment variables)."))
                    .weak(),
            );
            if reveal.connection.is_this_computer_only() {
                let note = match (&reveal.lan, address::normalize(&reveal.connection.url)) {
                    (Some(ip), Ok(url)) => tf(
                        l,
                        "This address works for a program on this computer only. On another device of your network, use http://{}:{} instead (if this computer's firewall lets it in). A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
                        &[ip, &url.port_or_default()],
                    ),
                    _ => t(
                        l,
                        "This address works for a program on this computer only. A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
                    )
                    .to_owned(),
                };
                ui.label(RichText::new(note).color(ui.visuals().warn_fg_color));
            }
            ui.add_space(6.0);
            if ui.button(t(l, "I have copied it: close")).clicked() {
                close = true;
            }
        });
        if close {
            // The token is wiped (Secret), and egui's copies of the field with it.
            session.reveal = None;
            ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(lines_field()));
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

/// Said after a secret was copied, the private way.
#[cfg(all(unix, not(target_os = "macos")))]
const COPIED: &str = "Copied. It stays in the clipboard until you copy something else. Clipboard managers are asked to skip it (KDE's does; some on GNOME do not). On Linux, what you copy may disappear when the editor closes: paste it first.";
#[cfg(not(all(unix, not(target_os = "macos"))))]
const COPIED: &str =
    "Copied. It stays in the clipboard until you copy something else; the clipboard history is asked to skip it.";
