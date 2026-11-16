//! The window: the start (this computer's server, or one elsewhere), then a
//! connected server's overview, applications, memory, limits and what it was
//! started with, and the tools.
//!
//! The window only draws and records clicks: the network, the password
//! store and the files of this computer are reached by jobs (see
//! [`crate::jobs`]), whose answers are taken at the next frame.

mod apps;
mod dialogs;
mod overview;
mod settings;
mod start;
mod tools;

#[cfg(test)]
pub(crate) use dialogs::MASK;
pub use tools::genkey;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};
use zeroize::Zeroizing;

use crate::address::{self, Class};
use crate::api::{
    self, ApiError, Application, Caps, Connected, Connection, LastConnection, Ping, Server, Settings, Status, Transport,
};
use crate::clip::{ArboardSink, Copied, CopySink, FakeSink};
use crate::format;
use crate::guard::{self, Failure};
use crate::i18n::{Lang, t, tf};
use crate::jobs::{Calls, Job, take};
use crate::local;
use crate::secret::Secret;
use crate::theme::{self, Mark};
use crate::vault::{self, KeyringVault, Vault};

/// What eframe keeps between two launches. No field can hold a secret: a
/// kept token is in the system's password store, and here only `token_kept`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The language chosen (`None`: the system's).
    pub lang: Option<Lang>,
    pub theme: Theme,
    /// The servers connected to, the last one first.
    pub servers: Vec<SavedServer>,
    /// The size of the text, in percent (100, 125, 150).
    pub text_size: u16,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs { lang: None, theme: Theme::System, servers: Vec::new(), text_size: 100 }
    }
}

/// A server connected to before.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SavedServer {
    /// Its address, normalized.
    pub address: String,
    /// Its management token is in the password store.
    pub token_kept: bool,
    /// Its token was read from this computer's settings file.
    pub local: bool,
}

/// Light, dark, or as the system.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Where the editor looks for a server installed on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalServer {
    /// Nowhere: the card says none was installed.
    None,
    /// Where this system's installer puts it.
    Installed,
    /// Under a folder of the tests.
    At(local::Paths),
}

/// What the editor works with: the password store, the clipboard, the
/// limits of the calls. The program takes the system's; the tests, theirs.
pub struct Setup {
    pub vault: Arc<dyn Vault>,
    pub clipboard: Box<dyn CopySink>,
    pub transport: Transport,
    /// The editor's folder (eframe's storage), where an early build left a `tokens.json`.
    pub storage_dir: Option<PathBuf>,
    /// Where to look for a server installed on this computer.
    pub local: LocalServer,
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
            local: LocalServer::Installed,
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
            local: LocalServer::None,
            connect_to: None,
        }
    }
}

/// The pages of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Overview,
    Applications,
    Memory,
    Limits,
    Startup,
    /// The server installed on this computer.
    Computer,
    Tools,
    About,
}

impl Section {
    /// Its icon in the side bar.
    fn icon(self) -> theme::Icon {
        match self {
            Section::Overview => theme::Icon::Overview,
            Section::Applications => theme::Icon::Applications,
            Section::Memory => theme::Icon::Memory,
            Section::Limits => theme::Icon::Limits,
            Section::Startup => theme::Icon::Startup,
            Section::Computer => theme::Icon::Computer,
            Section::Tools => theme::Icon::Tools,
            Section::About => theme::Icon::About,
        }
    }

    fn label(self, l: Lang) -> &'static str {
        match self {
            Section::Overview => t(l, "Overview"),
            Section::Applications => t(l, "Applications"),
            Section::Memory => t(l, "Memory"),
            Section::Limits => t(l, "Limits and visibility"),
            Section::Startup => t(l, "Started with"),
            Section::Computer => t(l, "This computer"),
            Section::Tools => t(l, "Tools"),
            Section::About => t(l, "About"),
        }
    }

    /// One line under the page's title: what the page is for.
    fn about(self, l: Lang) -> Option<&'static str> {
        match self {
            Section::Overview => Some(t(l, "Your server at a glance.")),
            Section::Applications => None,
            Section::Memory => Some(t(l, "How much memory Nuage DB may use, and for which application.")),
            Section::Limits => Some(t(l, "What an application may do, and who may reach the server.")),
            Section::Startup => Some(t(l, "What the server was started with.")),
            Section::Computer | Section::Tools | Section::About => None,
        }
    }
}

/// A message above the page: what went well, or what went wrong and what to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub mark: Mark,
    pub text: String,
}

impl Notice {
    fn ok(text: impl Into<String>) -> Notice {
        Notice { mark: Mark::Ok, text: text.into() }
    }

    fn warn(text: impl Into<String>) -> Notice {
        Notice { mark: Mark::Warn, text: text.into() }
    }

    fn bad(text: impl Into<String>) -> Notice {
        Notice { mark: Mark::Bad, text: text.into() }
    }

    /// It says something went wrong.
    pub fn is_bad(&self) -> bool {
        self.mark == Mark::Bad
    }
}

/// Where the token of a connection comes from.
enum Given {
    /// The field.
    Typed,
    /// The password store.
    Kept,
    /// The environment.
    Secret(Secret),
    /// This computer's settings file, read with an administrator's consent: kept if asked.
    Administrator(Secret),
    /// This computer's settings file, read by the person: read again next time, never kept.
    File(Secret),
}

/// The most servers "Servers you used" lists (those whose token is kept stay
/// until they are forgotten: their entry in the password store would be left behind).
const MAX_SERVERS: usize = 8;

/// A management token being written to the password store, once its
/// connection was taken (never before: a connection given up keeps nothing).
struct Keeping {
    address: String,
    job: Job<Result<(), String>>,
    /// "Forget everything" came meanwhile: what is written is deleted again.
    forgotten: Arc<AtomicBool>,
}

/// A read of the settings file of this computer, and, when it is for
/// administrators only, whether the system can ask for one (else why not).
type LocalRead = (Result<local::Found, local::ReadError>, Option<Result<(), String>>);

/// What the editor found on this computer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Here {
    /// A server the installer set up.
    installed: Option<local::Detected>,
    /// Nothing installed, but Nuage DB answers on port 8080 (Docker, by hand).
    answers_8080: bool,
}

/// The id of the token's field: its state (and its undo copies) is dropped
/// once connected.
fn token_field() -> egui::Id {
    egui::Id::unique("nuage-editor-management-token")
}

/// The help of the connection form: where the token is, by host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Host {
    #[default]
    Heroku,
    Installer,
    Docker,
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
    job: Option<Job<Connected>>,
    /// The step the connection running is at ([`api::Step`]).
    step: Arc<AtomicU8>,
    /// The connection running uses the kept token.
    with_kept: bool,
    /// The connection running was read from this computer's settings file.
    with_local: bool,
    /// The token of the connection running is kept once it is taken
    /// ("Remember on this computer", as it was ticked when it started).
    with_remember: bool,
    notice: Option<Notice>,
    /// The last connection failed: the ways on are offered.
    failed: bool,
    /// After a redirect to the same host over https: that address.
    upgrade: Option<String>,
    /// Reading the settings file of this computer.
    local_job: Option<Job<LocalRead>>,
    /// Stops the system's password window of the read running.
    local_stop: Option<local::Stop>,
    /// The page that says why the system will ask for an administrator is open.
    asking: bool,
    /// The settings file of a server of this computer is for administrators
    /// only, and whether the system can ask for one (else why not).
    needs_admin: Option<(PathBuf, Result<(), String>)>,
    /// What the last read of this computer's file found (said before connecting).
    read_note: Option<String>,
    help: Option<Host>,
    /// The field to focus at the next frame.
    focus: Option<egui::Id>,
    /// What "Try again" does.
    last: Option<Attempt>,
    /// The last attempt reached the server (its steps are shown).
    stepped: bool,
}

/// An attempt "Try again" repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempt {
    /// The address and the token of the form (or its kept token).
    Form,
    /// This computer's settings file, as the person.
    Here,
    /// This computer's settings file, with an administrator.
    Administrator,
}

/// What a change sent to the server is, for where its answer is said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Doing {
    Settings,
    Create,
    Token,
    Revoke,
    Memory,
    Origins,
}

/// What a change sent to the server comes back with.
enum Outcome {
    Settings(Settings),
    Created(Application, Connection, Lan),
    Token { app: String, read_only: bool, connection: Connection, lan: Lan },
    Revoked { app: String, id: String },
    Memory(Application),
    Origins(Application),
}

/// Why a new token is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Made {
    /// With its new application.
    Created,
    /// Another token of an application.
    Token,
}

/// A token shown once, in a window of its own, until its owner closes it.
struct Reveal {
    app: String,
    made: Made,
    read_only: bool,
    connection: Connection,
    /// How another device of the network reaches the server, when `NUAGE_URL` reaches this computer only.
    lan: Lan,
    /// The token is shown in clear (else masked).
    shown: bool,
    copied: Option<Copied>,
    /// The test of the token ([`Server::ping_with`]).
    test: Option<Job<Ping>>,
    tested: Option<Result<Ping, ApiError>>,
    /// "Close without copying?" was asked.
    closing: bool,
    /// The clipboard was emptied after a copy.
    cleared: bool,
}

impl Reveal {
    fn new(app: String, made: Made, read_only: bool, connection: Connection, lan: Lan) -> Reveal {
        Reveal {
            app,
            made,
            read_only,
            connection,
            lan,
            shown: false,
            copied: None,
            test: None,
            tested: None,
            closing: false,
            cleared: false,
        }
    }

    /// It was copied or shown: closing loses nothing unexpected.
    fn taken(&self) -> bool {
        self.copied.is_some() || self.shown
    }
}

/// A new token asked for an application: the tokens it had then, and
/// whether the answer was lost on the way (a token may have been made that
/// nobody has).
struct TokenAsked {
    app: String,
    known: Vec<String>,
    lost: bool,
    /// The list of applications was read again after the answer was lost:
    /// only then can the page say whether the token was made.
    checked: bool,
}

/// A token about to be revoked: its owner confirms.
struct Confirm {
    app: String,
    id: String,
    /// Focus went to "Keep it".
    focused: bool,
}

/// A server the editor is connected to.
struct Session {
    server: Server,
    /// Its token was read from this computer's settings file.
    local: bool,
    calls: Calls,
    section: Section,
    /// The application whose page is open (in Applications).
    open_app: Option<String>,
    settings: Option<Settings>,
    /// Why the settings could not be read (a server that has them).
    settings_error: Option<ApiError>,
    apps: Option<Vec<Application>>,
    status: Option<Status>,
    /// The last connection of each application that was not the editor's own
    /// test, as the editor saw it (the server keeps only the last one).
    sites: BTreeMap<String, LastConnection>,
    load_settings: Option<Job<Settings>>,
    load_apps: Option<Job<Vec<Application>>>,
    load_status: Option<Job<Status>>,
    /// Why the last refresh failed: the data shown is that of before, greyed.
    stale: Option<ApiError>,
    /// When the data was read: for "Updated 3 s ago" (this computer's clock).
    updated: Option<(Instant, i64)>,
    /// One change at a time.
    action: Option<(Doing, Job<Outcome>)>,
    notice: Option<Notice>,
    /// The fields of the settings, as typed (filled from the server's values).
    fields: BTreeMap<String, String>,
    public: bool,
    /// The memory fields of each application, as typed.
    app_fields: BTreeMap<String, (String, String)>,
    /// A new application: what it is for, its name, and whether the name was typed.
    new_what: String,
    new_name: String,
    name_typed: bool,
    /// Why the last creation failed, under its fields.
    new_error: Option<String>,
    /// The application of the last creation whose answer was lost: "Check" looks for it.
    unsure: Option<String>,
    /// The last new token asked.
    token_asked: Option<TokenAsked>,
    /// The website being added, and why the last change of websites failed.
    origin_field: String,
    origin_error: Option<String>,
    reveal: Option<Reveal>,
    confirm: Option<Confirm>,
}

impl Session {
    fn new(connected: Connected, local: bool, lang: Lang, calls: Calls) -> Session {
        let Connected { server, apps, settings, settings_error, status, status_error } = connected;
        let mut session = Session {
            server,
            local,
            calls,
            section: Section::Overview,
            open_app: None,
            settings: None,
            settings_error,
            apps: Some(apps),
            status: None,
            sites: BTreeMap::new(),
            load_settings: None,
            load_apps: None,
            load_status: None,
            // The status failed on the way: said, and asked again at Refresh.
            stale: status_error,
            updated: Some((Instant::now(), format::now_ms())),
            action: None,
            notice: None,
            fields: BTreeMap::new(),
            public: false,
            app_fields: BTreeMap::new(),
            new_what: String::new(),
            new_name: String::new(),
            name_typed: false,
            new_error: None,
            unsure: None,
            token_asked: None,
            origin_field: String::new(),
            origin_error: None,
            reveal: None,
            confirm: None,
        };
        if let Some(settings) = settings {
            session.take_settings(settings, lang);
        }
        if let Some(status) = status {
            session.take_status(status);
        }
        session
    }

    /// The server's status is in: the sites' connections are noted.
    fn take_status(&mut self, status: Status) {
        for db in &status.databases {
            if let Some(c) = db.last_connection.as_ref().filter(|c| !c.by_the_editor()) {
                self.sites.insert(db.name.clone(), c.clone());
            }
        }
        self.status = Some(status);
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
        for a in &settings.applications {
            self.app_fields.insert(a.name.clone(), (format::amount(lang, a.cache_mb), format::amount(lang, a.kv_mb)));
        }
        self.settings = Some(settings);
        self.settings_error = None;
        self.public_as_saved();
    }

    /// The "Public status" box as the server has it (also after a change of it that failed).
    fn public_as_saved(&mut self) {
        self.public = self.settings.as_ref().and_then(|s| s.get("status.public")).is_some_and(|s| s.shown() == "true");
    }

    /// Reads again what the pages show (after a change, on Refresh, on focus).
    fn reload(&mut self, ctx: &egui::Context) {
        let caps = self.server.caps().clone();
        if self.load_settings.is_none() && caps.settings {
            let server = self.server.clone();
            self.load_settings = Some(Job::start(ctx, &self.calls, move || server.settings()));
        }
        if self.load_apps.is_none() {
            let server = self.server.clone();
            self.load_apps = Some(Job::start(ctx, &self.calls, move || server.applications()));
        }
        if self.load_status.is_none() && caps.status {
            let server = self.server.clone();
            self.load_status = Some(Job::start(ctx, &self.calls, move || server.status()));
        }
    }

    fn loading(&self) -> bool {
        self.load_apps.is_some() || self.load_settings.is_some() || self.load_status.is_some()
    }

    fn busy(&self) -> bool {
        self.action.is_some()
    }

    fn act(
        &mut self,
        ctx: &egui::Context,
        doing: Doing,
        work: impl FnOnce(Server) -> Result<Outcome, ApiError> + Send + 'static,
    ) {
        if self.busy() {
            return;
        }
        let server = self.server.clone();
        self.notice = None;
        self.action = Some((doing, Job::start(ctx, &self.calls, move || work(server))));
    }

    /// The application named `name`, as last read.
    fn app(&self, name: &str) -> Option<&Application> {
        self.apps.as_ref()?.iter().find(|a| a.name == name)
    }

    /// The websites allowed to call the application `name`, as last read.
    fn websites_of(&self, name: &str) -> Vec<String> {
        self.app(name).map(|a| a.origins.clone()).unwrap_or_default()
    }

    /// The server is reached without encryption over the Internet (a
    /// website's https page cannot call it then).
    fn cleartext_public(&self) -> bool {
        address::normalize(self.server.address()).is_ok_and(|a| a.is_cleartext_public())
    }
}

/// How another device of the network reaches a server whose `NUAGE_URL`
/// reaches this computer only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Lan {
    /// Not known (or not asked: the address is not this computer's).
    Unknown,
    /// This computer's address on its network.
    At(String),
    /// The server listens on this computer only (`NUAGE_BIND=127.0.0.1`).
    HereOnly(String),
}

/// What the server listens on, when it says a loopback address
/// (`NUAGE_BIND` in its startup settings): it answers this computer only.
pub(super) fn bound_here(settings: Option<&Settings>) -> Option<String> {
    let bind = settings?.startup.iter().find(|s| s.variable == "NUAGE_BIND")?.value.clone()?;
    let host = bind.trim().trim_start_matches('[').trim_end_matches(']').to_owned();
    let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    loopback.then_some(bind)
}

/// How another device reaches the server, for a `NUAGE_URL` that reaches
/// this computer only (run in the job that made the token).
fn lan_for(connection: &Connection, bound_here: Option<&str>) -> Lan {
    if !connection.is_this_computer_only() {
        return Lan::Unknown;
    }
    if let Some(bind) = bound_here {
        return Lan::HereOnly(bind.to_owned());
    }
    address::lan_ip().map_or(Lan::Unknown, |ip| Lan::At(ip.to_string()))
}

/// A generated token (Tools, and the help of the connection form): shown
/// once, masked until asked.
struct Generated {
    token: Secret,
    shown: bool,
    copied: Option<Copied>,
}

/// A kept token forgotten: its address, whether its server goes from the
/// list once it is, and the store's answer.
type Forgetting = (String, bool, Result<(), String>);

/// How forgetting each kept token went: its address, and the store's answer.
type Forgotten = Vec<(String, Result<(), String>)>;

/// The test of an application's token (Tools).
#[derive(Default)]
struct TokenTest {
    address: String,
    token: Zeroizing<String>,
    /// The address whose plain http was accepted, for the next test only.
    cleartext_ok: Option<String>,
    job: Option<Job<Ping>>,
    answer: Option<Result<Ping, ApiError>>,
}

/// Starting the server of this computer, then waiting for it to answer.
struct Starting {
    job: Job<Result<bool, local::ReadError>>,
    stop: local::Stop,
}

/// The editor's window.
pub struct Editor {
    pub prefs: Prefs,
    vault: Arc<dyn Vault>,
    /// Whether tokens can be kept (`None`: still asking the system).
    vault_status: Option<Result<(), String>>,
    vault_job: Option<Job<Result<(), String>>>,
    /// A kept token being forgotten.
    forget_job: Option<Job<Forgetting>>,
    /// Everything being forgotten (About): how it went for each address.
    forget_all_job: Option<Job<Forgotten>>,
    /// The tokens of the servers just connected to, being kept (each is
    /// waited for: one dropped would be kept with nothing in the window to forget it).
    keeping: Vec<Keeping>,
    old_file_job: Option<Job<Result<bool, String>>>,
    clipboard: Box<dyn CopySink>,
    transport: Transport,
    calls: Calls,
    storage_dir: Option<PathBuf>,
    local: LocalServer,
    /// What is installed on this computer (`None`: still looking).
    here: Option<Here>,
    here_job: Option<Job<Here>>,
    /// The jobs of the first frame were started.
    started: bool,
    connect_to: Option<(String, Secret)>,
    connect: ConnectForm,
    session: Option<Session>,
    /// A page shown while not connected (Tools, About).
    aside: Option<Section>,
    generated: Option<Generated>,
    token_test: TokenTest,
    starting: Option<Starting>,
    /// What the last start of this computer's server said.
    start_note: Option<Notice>,
    /// A saved server whose forgetting waits for a confirmation.
    forget_ask: Option<SavedServer>,
    /// "Forget everything on this computer" waits for a confirmation.
    forget_all_ask: bool,
    /// The window was asked to close while something must not be lost.
    quit_ask: bool,
    /// The person chose to quit anyway.
    quitting: bool,
    /// What the window was last given (theme, title, text size).
    theme_applied: Option<Theme>,
    title_applied: Option<Lang>,
    size_applied: Option<u16>,
    /// The window had the focus at the last frame.
    focused: bool,
    /// A bug stopped a frame: the window shows it, instead of its page.
    failure: Option<Failure>,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // What it shows, never a token.
        f.debug_struct("Editor")
            .field("prefs", &self.prefs)
            .field("address", &self.connect.address)
            .field("server", &self.session.as_ref().map(|s| &s.server))
            .field("generated", &self.generated.as_ref().map(|g| &g.token))
            .finish_non_exhaustive()
    }
}

impl Editor {
    /// The editor with what was remembered (`prefs`), working with `setup`.
    /// Nothing is read or called before the first frame.
    pub fn new(prefs: Prefs, setup: Setup) -> Editor {
        let mut connect = ConnectForm::default();
        if let Some(last) = prefs.servers.iter().find(|s| !s.local).or(prefs.servers.first()) {
            connect.address = last.address.clone();
            connect.use_kept = last.token_kept;
        }
        Editor {
            prefs,
            vault: setup.vault,
            vault_status: None,
            vault_job: None,
            forget_job: None,
            forget_all_job: None,
            keeping: Vec::new(),
            old_file_job: None,
            clipboard: setup.clipboard,
            transport: setup.transport,
            calls: Calls::default(),
            storage_dir: setup.storage_dir,
            local: setup.local,
            here: None,
            here_job: None,
            started: false,
            connect_to: setup.connect_to,
            connect,
            session: None,
            aside: None,
            generated: None,
            token_test: TokenTest::default(),
            starting: None,
            start_note: None,
            forget_ask: None,
            forget_all_ask: false,
            quit_ask: false,
            quitting: false,
            theme_applied: None,
            title_applied: None,
            size_applied: None,
            focused: true,
            failure: None,
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

    /// The page shown: a section of the server connected to, or one opened
    /// before connecting (`None`: the start).
    pub fn section(&self) -> Option<Section> {
        self.session.as_ref().map(|s| s.section).or(self.aside)
    }

    /// What the server connected to has.
    pub fn caps(&self) -> Option<&Caps> {
        self.session.as_ref().map(|s| s.server.caps())
    }

    /// Whether tokens can be kept on this computer (`None`: still asking).
    pub fn can_keep_tokens(&self) -> Option<&Result<(), String>> {
        self.vault_status.as_ref()
    }

    /// Whether a call for the connected server (to it, or keeping its token)
    /// is still on its way.
    pub fn waiting_for_server(&self) -> bool {
        !self.keeping.is_empty() || self.session.as_ref().is_some_and(|s| s.busy() || s.loading())
    }

    /// Whether the search for a server installed on this computer is over.
    pub fn looked_here(&self) -> bool {
        self.here.is_some()
    }

    /// One frame of the window. A bug (a panic) in it does not close the
    /// window: the frame is dropped, and the window shows what failed.
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        self.frame_with(ui, Editor::show);
    }

    /// [`Editor::frame`], drawn by `draw` (the tests make it fail on purpose).
    #[doc(hidden)]
    pub fn frame_with(&mut self, ui: &mut egui::Ui, draw: impl FnOnce(&mut Editor, &mut egui::Ui)) {
        if let Some(failure) = self.failure.clone() {
            // The failure page itself failing leaves a page of text alone, never a closed window.
            if guard::catch(|| self.failure_page(ui, &failure)).is_err() {
                let _ = guard::catch(|| {
                    egui::CentralPanel::default().show(ui, |ui| ui.label(failure.details()));
                });
            }
            return;
        }
        if let Err(failure) = guard::catch(|| draw(self, ui)) {
            self.failure = Some(failure);
            ui.ctx().request_repaint();
        }
    }

    /// The bug that stopped the window's page, if one did.
    pub fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// What the window shows after a bug: what failed, and the ways on.
    fn failure_page(&mut self, ui: &mut egui::Ui, failure: &Failure) {
        let l = self.lang();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(16.0);
            ui.heading(t(l, "The editor ran into a bug"));
            ui.label(t(
                l,
                "It stopped what it was showing. A change already sent to the server may or may not have been carried out: check after reconnecting.",
            ));
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                if ui.add(theme::primary(t(l, "Back to the start"), ui)).clicked() {
                    self.back_to_start();
                }
                if ui.button(t(l, "Copy the details")).clicked() {
                    ui.ctx().copy_text(failure.details());
                }
                if ui.button(t(l, "Quit")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.add_space(8.0);
            egui::CollapsingHeader::new(t(l, "Details")).id_salt("failure-details").default_open(true).show(ui, |ui| {
                ui.label(RichText::new(tf(l, "What failed: {}", &[&failure.message])).monospace());
                if let Some(place) = &failure.place {
                    ui.label(RichText::new(tf(l, "Where: {}", &[place])).monospace().weak());
                }
                ui.label(RichText::new(failure.details()).monospace().weak());
            });
            ui.add_space(8.0);
            ui.hyperlink_to(
                t(l, "Tell the editor's authors (GitHub issues)"),
                "https://github.com/maxlestage/Nuage/issues",
            );
        });
    }

    /// The editor as it was opened, with its preferences, its password store
    /// and its clipboard: what the bug left behind is dropped.
    fn back_to_start(&mut self) {
        let setup = Setup {
            vault: self.vault.clone(),
            clipboard: std::mem::replace(&mut self.clipboard, Box::new(FakeSink::default())),
            transport: self.transport.clone(),
            storage_dir: self.storage_dir.clone(),
            local: self.local.clone(),
            connect_to: None,
        };
        *self = Editor::new(self.prefs.clone(), setup);
    }

    /// The whole window.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let l = self.lang();
        if !std::mem::replace(&mut self.started, true) {
            theme::install(&ctx);
            self.start(&ctx);
        }
        self.apply_prefs(&ctx, l);
        self.poll(&ctx);
        self.on_focus(&ctx);
        self.close_guard(&ctx);
        if self.waiting() {
            // The seconds waited, and "Stop waiting", show while nothing else repaints.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let p = *theme::of(ui);
        let chrome = |inner: egui::Margin| {
            egui::Frame::new().fill(p.chrome).inner_margin(inner).stroke(egui::Stroke::new(1.0, p.line))
        };
        egui::Panel::top("top").frame(chrome(egui::Margin::symmetric(16, 8))).show(ui, |ui| self.top_bar(ui, l));
        if self.session.is_some() {
            // Narrower in a small window, so that the page keeps its room.
            let width = if ui.available_width() < 760.0 { 196.0 } else { 236.0 };
            egui::Panel::left("sections")
                .resizable(false)
                .exact_size(width)
                .frame(chrome(egui::Margin::symmetric(12, 14)))
                .show(ui, |ui| {
                    // Its last buttons stay within reach in a small window, or with a larger text.
                    egui::ScrollArea::vertical().auto_shrink([true, false]).show(ui, |ui| self.sections(ui, l));
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin {
                left: 28,
                right: 20,
                top: 22,
                bottom: 16,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    // 920 at most, for lines that read well; never wider than the window
                    // (this scrolls only downwards: what is past its right edge is lost).
                    let width = (ui.available_width() - 8.0).min(920.0);
                    // Without the sidebar, the page sits in the middle of the window.
                    let side =
                        if self.session.is_none() { ((ui.available_width() - 8.0 - width) / 2.0).floor() } else { 0.0 };
                    if side >= 1.0 {
                        ui.horizontal_top(|ui| {
                            ui.add_space(side);
                            ui.vertical(|ui| {
                                ui.set_max_width(width);
                                self.page(ui, l);
                            });
                        });
                    } else {
                        ui.set_max_width(width);
                        self.page(ui, l);
                    }
                    ui.add_space(24.0);
                });
            });
        self.dialogs(&ctx, l);
    }

    /// The page of the central panel.
    fn page(&mut self, ui: &mut egui::Ui, l: Lang) {
        let section = match &self.session {
            Some(session) => Some(session.section),
            None => self.aside,
        };
        match section {
            None => self.start_page(ui, l),
            Some(Section::Tools) => self.tools_page(ui, l),
            Some(Section::About) => self.about_page(ui, l),
            Some(Section::Computer) => self.computer_page(ui, l),
            Some(_) => {
                let here = self.here.clone().unwrap_or_default();
                if let Some(session) = self.session.as_mut() {
                    session_page(ui, session, &here, l);
                }
            }
        }
    }

    /// The language, the theme and the text size of the preferences, given
    /// to the window when they change.
    fn apply_prefs(&mut self, ctx: &egui::Context, l: Lang) {
        // Only when it changes: setting it asks for another frame.
        if self.theme_applied != Some(self.prefs.theme) {
            ctx.set_theme(match self.prefs.theme {
                Theme::System => egui::ThemePreference::System,
                Theme::Light => egui::ThemePreference::Light,
                Theme::Dark => egui::ThemePreference::Dark,
            });
            self.theme_applied = Some(self.prefs.theme);
        }
        if self.title_applied != Some(l) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t(l, "Nuage DB Editor").to_owned()));
            self.title_applied = Some(l);
        }
        let size = match self.prefs.text_size {
            125 | 150 => self.prefs.text_size,
            _ => 100,
        };
        if self.size_applied != Some(size) {
            ctx.set_zoom_factor(f32::from(size) / 100.0);
            self.size_applied = Some(size);
        }
    }

    /// Reads the server again when the window comes back to the front with
    /// data older than a minute (there is no polling in the background).
    fn on_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.focused);
        let back = focused && !std::mem::replace(&mut self.focused, focused);
        if back
            && let Some(session) = self.session.as_mut()
            && session.updated.is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(60))
        {
            session.reload(ctx);
        }
    }

    /// Never closes silently while a change is on its way or a new token was not copied.
    fn close_guard(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quitting {
            return;
        }
        let Some(session) = &self.session else { return };
        if session.busy() || session.reveal.as_ref().is_some_and(|r| !r.taken()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.quit_ask = true;
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
        self.look_here(ctx);
        if let Some((address, token)) = self.connect_to.take() {
            self.connect.address = address;
            self.connect.use_kept = false;
            self.start_connect(ctx, Given::Secret(token));
        }
    }

    /// Looks for a server on this computer, on a job (it reads files and calls a port).
    fn look_here(&mut self, ctx: &egui::Context) {
        let paths = match &self.local {
            LocalServer::None => {
                self.here = Some(Here::default());
                return;
            }
            LocalServer::Installed => local::Paths::installed(),
            LocalServer::At(paths) => Some(paths.clone()),
        };
        let probe = self.local == LocalServer::Installed;
        self.here_job = Some(Job::start(ctx, &self.calls, move || {
            let installed = paths.and_then(|p| local::detect(p, local::health_answers));
            let answers_8080 = probe && installed.is_none() && local::health_answers(local::DEFAULT_PORT);
            Ok(Here { installed, answers_8080 })
        }));
    }

    fn waiting(&self) -> bool {
        self.connect.job.is_some()
            || self.connect.local_job.is_some()
            || self.vault_job.is_some()
            || !self.keeping.is_empty()
            || self.starting.is_some()
            || self.session.as_ref().is_some_and(|s| s.busy() || s.loading())
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
                    self.connect.notice = Some(Notice::warn(t(
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
        if let Some(answer) = take(&mut self.here_job) {
            self.here = Some(answer.unwrap_or_default());
        }
        self.poll_forget(l);
        self.poll_tools(ctx, l);
        if let Some(answer) = take(&mut self.connect.job) {
            self.connected(ctx, answer);
        }
        self.poll_keeping(l);
        if let Some(answer) = take(&mut self.connect.local_job) {
            self.connect.local_stop = None;
            match answer {
                Ok((Ok(found), _)) => self.use_local(ctx, found),
                Ok((Err(local::ReadError::NeedsAdministrator(file)), can_ask)) => {
                    self.connect.needs_admin = Some((file, can_ask.unwrap_or(Ok(()))));
                    self.connect.asking = true;
                    // Ticked on this card, where it is shown (else the system asks at every
                    // start); unticked again once connected, or when the card is cancelled.
                    self.connect.remember = true;
                }
                Ok((Err(e), _)) => {
                    self.connect.failed = true;
                    self.connect.notice = Some(Notice::bad(local_error(l, e)));
                }
                Err(e) => self.connect.notice = Some(Notice::bad(e.explain(l))),
            }
        }
        let Some(session) = self.session.as_mut() else { return };
        let mut refused = false;
        let mut done = false;
        if let Some(answer) = take(&mut session.load_settings) {
            done = true;
            match answer {
                Ok(settings) => session.take_settings(settings, l),
                Err(ApiError::TokenRefused) => refused = true,
                Err(e) => {
                    if session.settings.is_none() {
                        session.settings_error = Some(e.clone());
                    }
                    session.stale = Some(e);
                }
            }
        }
        if let Some(answer) = take(&mut session.load_apps) {
            done = true;
            match answer {
                Ok(apps) => {
                    session.apps = Some(apps);
                    if let Some(asked) = session.token_asked.as_mut().filter(|a| a.lost) {
                        asked.checked = true;
                    }
                    session.stale = None;
                }
                Err(ApiError::TokenRefused) => refused = true,
                Err(e) => session.stale = Some(e),
            }
        }
        if let Some(answer) = take(&mut session.load_status) {
            done = true;
            match answer {
                Ok(status) => {
                    session.server.learned_status(Some(status.version.clone()));
                    session.take_status(status);
                }
                // Never read since connecting, and the server's own no: an older server after all.
                Err(e) if session.status.is_none() && e.from_the_server() => session.server.learned_status(None),
                Err(ApiError::TokenRefused) => refused = true,
                Err(e) => session.stale = Some(e),
            }
        }
        if done && !session.loading() && session.stale.is_none() {
            session.updated = Some((Instant::now(), format::now_ms()));
        }
        if let Some((doing, answer)) = session.action.as_ref().and_then(|(d, job)| Some((*d, job.poll()?))) {
            session.action = None;
            match answer {
                Ok(outcome) => took(ctx, session, outcome, l),
                Err(ApiError::TokenRefused) => refused = true,
                Err(e) if e.change_unknown() => unknown_change(ctx, session, doing, &e, l),
                Err(e) => failed_change(ctx, session, doing, &e, l),
            }
        }
        if refused {
            // Back to the start: the token was changed on the server.
            let (address, local) = (session.server.address().to_owned(), session.local);
            self.session = None;
            self.connect.address = address;
            self.connect.use_kept = false;
            self.connect.failed = true;
            if local {
                // A token kept for it is the old one: Open reads the file again, not the store.
                // (The store keeps tokens by address: the next one kept replaces it; refused, it
                // opens nothing meanwhile.)
                for s in self.prefs.servers.iter_mut().filter(|s| s.local) {
                    s.token_kept = false;
                }
            }
            self.connect.notice = Some(Notice::bad(if local {
                // Its token was read from the settings file, never typed: read it again.
                t(
                    l,
                    "The server no longer accepts this token: it was changed in its settings file. Open it again from On this computer: the editor reads the new token from the file.",
                )
            } else {
                t(l, "The server no longer accepts this token: it was changed on the server. Type the new one.")
            }));
        }
    }

    /// The answers of the password store, for the tokens of the servers just connected to.
    fn poll_keeping(&mut self, l: Lang) {
        let mut answers = Vec::new();
        self.keeping.retain(|k| match k.job.poll() {
            Some(answer) => {
                answers.push((k.address.clone(), answer, k.forgotten.load(Ordering::SeqCst)));
                false
            }
            None => true,
        });
        for (address, answer, forgotten) in answers {
            let why = match answer {
                // Forgotten while it was written: the job deleted it again, it is not kept.
                Ok(Ok(())) if forgotten => continue,
                Ok(Ok(())) => {
                    for s in self.prefs.servers.iter_mut().filter(|s| s.address == address) {
                        s.token_kept = true;
                    }
                    if self.typed_base().as_deref() == Some(address.as_str()) {
                        self.connect.use_kept = true;
                    }
                    continue;
                }
                Ok(Err(why)) => why,
                Err(e) => e.explain(l),
            };
            let notice = Notice::warn(tf(
                l,
                "Connected, but the token could not be kept in {} ({}): it is used until you close the editor.",
                &[&self.vault.store().place(l), &why],
            ));
            match self.session.as_mut().filter(|s| s.server.address() == address) {
                Some(session) => session.notice = Some(notice),
                None => self.connect.notice = Some(notice),
            }
        }
    }

    /// Writes the token of `server`, just connected to, in the password store (on a job:
    /// the system may show an Allow window, or ask to unlock the keyring).
    fn keep(&mut self, ctx: &egui::Context, server: &Server) {
        let (vault, server) = (self.vault.clone(), server.clone());
        let address = server.address().to_owned();
        let forgotten = Arc::new(AtomicBool::new(false));
        let undo = forgotten.clone();
        let job = Job::start(ctx, &self.calls, move || {
            let kept = vault.set(server.address(), server.token());
            // "Forget everything" came while it was written: its own delete may have run first.
            if undo.load(Ordering::SeqCst) {
                let _ = vault.delete(server.address());
            }
            Ok(kept)
        });
        self.keeping.push(Keeping { address, job, forgotten });
    }

    /// The answers of the jobs that forget kept tokens.
    fn poll_forget(&mut self, l: Lang) {
        if let Some(answer) = take(&mut self.forget_job) {
            match answer {
                Ok((address, drop, Ok(()))) => {
                    if drop {
                        self.prefs.servers.retain(|s| s.address != address);
                    }
                    for s in self.prefs.servers.iter_mut().filter(|s| s.address == address) {
                        s.token_kept = false;
                    }
                    self.connect.notice =
                        Some(Notice::ok(t(l, "Token forgotten: it is no longer kept on this computer.")));
                }
                // The server stays in the list, its token marked kept: Forget can be pressed again.
                Ok((_, _, Err(why))) => {
                    self.connect.notice =
                        Some(Notice::bad(tf(l, "The password store did not forget the token: {}", &[&why])))
                }
                Err(e) => self.connect.notice = Some(Notice::bad(e.explain(l))),
            }
        }
        if let Some(answer) = take(&mut self.forget_all_job) {
            let store = self.vault.store().place(l);
            let failed: Vec<String> = match answer {
                Ok(results) => results
                    .into_iter()
                    .filter_map(|(address, done)| done.err().map(|why| format!("{address} ({why})")))
                    .collect(),
                Err(e) => vec![e.explain(l)],
            };
            self.start_note = Some(if failed.is_empty() {
                Notice::ok(t(l, "Everything the editor kept on this computer is forgotten."))
            } else {
                Notice::bad(tf(
                    l,
                    "The token of {} could not be removed: it is still in {}. Remove the entry io.github.maxlestage.nuage-editor yourself in Keychain Access, Credential Manager or your keyring.",
                    &[&failed.join(", "), &store],
                ))
            });
        }
    }

    /// A connection came back.
    fn connected(&mut self, ctx: &egui::Context, answer: Result<Connected, ApiError>) {
        let l = self.lang();
        let with_kept = std::mem::take(&mut self.connect.with_kept);
        let with_local = std::mem::take(&mut self.connect.with_local);
        let with_remember = std::mem::take(&mut self.connect.with_remember);
        match answer {
            Ok(connected) => {
                let address = connected.server.address().to_owned();
                // Counted as kept once the store says it is (see poll_keeping).
                let token_kept = with_kept || self.saved(&address).is_some_and(|s| s.token_kept);
                if with_remember {
                    self.keep(ctx, &connected.server);
                }
                let local = with_local || self.saved(&address).is_some_and(|s| s.local);
                self.prefs.servers.retain(|s| s.address != address);
                self.prefs.servers.insert(0, SavedServer { address: address.clone(), token_kept, local });
                // The oldest go first; never the one just used.
                while self.prefs.servers.len() > MAX_SERVERS {
                    let oldest = self.prefs.servers.iter().enumerate().skip(1).rev().find(|(_, s)| !s.token_kept);
                    match oldest.map(|(i, _)| i) {
                        Some(i) => {
                            self.prefs.servers.remove(i);
                        }
                        None => break,
                    }
                }
                // The typed token is wiped (Zeroizing), and egui's state of its field with it.
                self.connect.token = Zeroizing::new(String::new());
                ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(token_field()));
                self.connect.address = address;
                self.connect.show_token = false;
                self.connect.cleartext_ok = None;
                self.connect.use_kept = token_kept;
                // A choice for that connection: the next one asks again.
                self.connect.remember = false;
                self.connect.notice = None;
                self.connect.failed = false;
                self.connect.read_note = None;
                self.connect.asking = false;
                self.aside = None;
                self.session = Some(Session::new(connected, local, l, self.calls.clone()));
            }
            Err(ApiError::TokenRefused) if with_kept => {
                // Kept until a new one works: never deleted without a word.
                self.connect.use_kept = false;
                self.connect.remember = true;
                self.connect.failed = true;
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
                let mut text = e.explain(l);
                if e == ApiError::TokenRefused
                    && self.connect.token.trim_start_matches(|c: char| !c.is_ascii_alphanumeric()).starts_with("nuage_")
                {
                    text = format!("{text} {}", t(l, "This one looks like an application's token."));
                }
                self.connect.failed = true;
                self.connect.notice = Some(Notice::bad(text));
            }
        }
    }

    /// The settings file of the server of this computer was read.
    fn use_local(&mut self, ctx: &egui::Context, found: local::Found) {
        let l = self.lang();
        let local::Found { file, address, port, site_url, token, via, .. } = found;
        self.connect.needs_admin = None;
        self.connect.asking = false;
        self.connect.address = address.clone();
        self.connect.use_kept = false;
        let note = match &site_url {
            Some(site) => tf(l, "Read: port {}, management token found, public address {}.", &[&port, site]),
            None => tf(l, "Read: port {}, management token found.", &[&port]),
        };
        self.connect.public = site_url.map(|site| (address, site));
        match (token, via) {
            (Some(token), local::Via::File) => {
                self.connect.read_note = Some(note);
                // The person can read the file again next time: no copy goes in the password store.
                self.start_connect(ctx, Given::File(token));
            }
            (Some(token), local::Via::Administrator) => {
                self.connect.read_note = Some(note);
                self.start_connect(ctx, Given::Administrator(token));
            }
            (None, _) => {
                self.connect.failed = true;
                self.connect.notice = Some(Notice::bad(no_token(l, &file)));
            }
        }
    }

    fn start_connect(&mut self, ctx: &egui::Context, given: Given) {
        let l = self.lang();
        let address = match address::normalize(&self.connect.address) {
            Ok(a) => a,
            Err(e) => {
                self.connect.notice = Some(Notice::bad(start::address_error(l, &self.connect.address, &e)));
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
        let keep = matches!(given, Given::Typed | Given::Administrator(_));
        let with_local = matches!(given, Given::File(_) | Given::Administrator(_));
        if matches!(given, Given::Typed | Given::Kept) {
            self.connect.last = Some(Attempt::Form);
        }
        let token = match given {
            Given::Secret(token) | Given::File(token) | Given::Administrator(token) => Some(token),
            Given::Kept => None,
            Given::Typed => match api::typed_token(&self.connect.token) {
                Ok(token) => Some(token),
                Err(e) => {
                    // Refused before anything is sent: the steps of an earlier try are not this one's.
                    self.connect.stepped = false;
                    self.connect.notice = Some(Notice::bad(e.explain(l)));
                    return;
                }
            },
        };
        let remember = keep && self.connect.remember && token.is_some() && matches!(self.vault_status, Some(Ok(())));
        let public = self.connect.public.as_ref().filter(|(a, _)| *a == base).map(|(_, site)| site.clone());
        let (vault, transport) = (self.vault.clone(), self.transport.clone());
        let step = Arc::new(AtomicU8::new(0));
        self.connect.step = step.clone();
        self.connect.with_kept = token.is_none();
        self.connect.with_local = with_local;
        // Kept once it worked and its answer is taken: "Stop waiting" keeps nothing.
        self.connect.with_remember = remember;
        self.connect.notice = None;
        self.connect.failed = false;
        self.connect.stepped = true;
        self.connect.upgrade = None;
        self.connect.job = Some(Job::start(ctx, &self.calls, move || {
            let token = match token {
                Some(token) => token,
                None => vault.get(&base).map_err(ApiError::Keystore)?.ok_or(ApiError::NotKept)?,
            };
            Server::new(&base, token, l, &transport)?.with_public(public).connect_reporting(&step)
        }));
    }

    /// Reads this computer's settings file (no system window: it is the
    /// person's, or it says it is for administrators only).
    fn read_here(&mut self, ctx: &egui::Context) {
        let paths = match &self.local {
            LocalServer::None => return,
            LocalServer::Installed => None,
            LocalServer::At(paths) => Some(paths.clone()),
        };
        self.connect.notice = None;
        self.connect.failed = false;
        self.connect.stepped = false;
        self.connect.read_note = None;
        self.connect.last = Some(Attempt::Here);
        self.connect.local_job = Some(Job::start(ctx, &self.calls, move || {
            let read = match &paths {
                Some(paths) => local::read_at(paths),
                None => local::read(),
            };
            // Looked up here: it looks at files.
            let can_ask =
                matches!(read, Err(local::ReadError::NeedsAdministrator(_))).then(local::can_ask_administrator);
            Ok((read, can_ask))
        }));
    }

    /// Asks the system for an administrator, then reads the four lines.
    fn read_as_administrator(&mut self, ctx: &egui::Context) {
        let l = self.lang();
        let stop = local::Stop::default();
        self.connect.local_stop = Some(stop.clone());
        self.connect.notice = None;
        self.connect.failed = false;
        self.connect.stepped = false;
        self.connect.last = Some(Attempt::Administrator);
        self.connect.local_job =
            Some(Job::start(ctx, &self.calls, move || Ok((local::read_as_administrator(l, &stop), None))));
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, l: Lang) {
        // The language and the theme where there is room (About has them too): the
        // address and its lock always show, never under them.
        let roomy = ui.available_width() >= TOP_BAR_ROOMY;
        let connected = self.session.as_ref().map(|s| (s.server.address().to_owned(), s.busy()));
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((_, busy)) = connected {
                    // Not while a change is on its way: its answer would be lost.
                    let button = ui
                        .add_enabled(!busy, egui::Button::new(t(l, "Disconnect")))
                        .on_disabled_hover_text(t(l, "A change is on its way: wait for its answer."));
                    if button.clicked() {
                        self.session = None;
                    }
                }
                if roomy {
                    self.theme_combo(ui, l);
                    self.lang_combo(ui, l, true);
                }
                // What is left of the width, wrapped onto a second line when it must.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true), |ui| {
                    // The editor's name where it leaves room for the connection's pill.
                    if roomy && (connected.is_none() || ui.available_width() >= 640.0) {
                        theme::logo(ui, 22.0);
                        ui.label(theme::strong(t(l, "Nuage DB Editor")).size(16.0));
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);
                    }
                    match &connected {
                        Some((address, _)) => {
                            let p = *theme::of(ui);
                            let (m, lock, why) = lock(l, address);
                            egui::Frame::new()
                                .fill(p.card)
                                .stroke(egui::Stroke::new(1.0, p.line))
                                .corner_radius(egui::CornerRadius::same(255))
                                .inner_margin(egui::Margin::symmetric(12, 5))
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    ui.horizontal(|ui| {
                                        theme::mark(ui, Mark::On);
                                        ui.label(tf(l, "Connected to {}", &[&host_of(address)]));
                                        ui.label(RichText::new("·").color(p.muted));
                                        // A short word in the bar, the sentence on hover.
                                        theme::mark(ui, m);
                                        ui.label(RichText::new(lock).color(m.color(&p))).on_hover_text(why);
                                    });
                                });
                        }
                        None => {
                            theme::mark(ui, Mark::Off);
                            ui.label(RichText::new(t(l, "Not connected")).color(theme::of(ui).muted));
                        }
                    }
                });
            });
        });
    }

    fn theme_combo(&mut self, ui: &mut egui::Ui, l: Lang) {
        ui.name_inputs(t(l, "Theme"), |ui| {
            let short = match self.prefs.theme {
                Theme::System => t(l, "Auto"),
                Theme::Light => t(l, "Light"),
                Theme::Dark => t(l, "Dark"),
            };
            egui::ComboBox::from_id_salt("theme").selected_text(short).show_ui(ui, |ui| {
                for theme in [Theme::System, Theme::Light, Theme::Dark] {
                    ui.selectable_value(&mut self.prefs.theme, theme, theme_label(l, theme));
                }
            });
        });
    }

    /// The language; `short`: the combo says only the language in use, in its own words.
    fn lang_combo(&mut self, ui: &mut egui::Ui, l: Lang, short: bool) {
        let selected = if short { lang_label(l, Some(l)) } else { lang_label(l, self.prefs.lang) };
        ui.name_inputs(t(l, "Language"), |ui| {
            egui::ComboBox::from_id_salt("lang").selected_text(selected).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.prefs.lang, None, lang_label(l, None));
                for lang in Lang::ALL {
                    ui.selectable_value(&mut self.prefs.lang, Some(lang), lang_label(l, Some(lang)));
                }
            });
        });
    }

    fn sections(&mut self, ui: &mut egui::Ui, l: Lang) {
        let has_here = self.here.as_ref().is_some_and(|h| h.installed.is_some());
        let Some(session) = self.session.as_mut() else { return };
        ui.spacing_mut().item_spacing.y = 3.0;
        ui.add_space(2.0);
        for section in [
            Section::Overview,
            Section::Applications,
            Section::Memory,
            Section::Limits,
            Section::Startup,
            Section::Computer,
            Section::Tools,
            Section::About,
        ] {
            if section == Section::Computer && !(session.local && has_here) {
                continue;
            }
            if theme::nav_item(ui, section.icon(), section.label(l), session.section == section).clicked() {
                session.section = section;
                session.open_app = None;
                session.notice = None;
            }
        }
        ui.add_space(14.0);
        ui.label(RichText::new(t(l, "Shortcuts")).size(12.0).color(theme::of(ui).muted));
        ui.add_space(2.0);
        if theme::nav_item(ui, theme::Icon::Console, t(l, "Web console"), false).clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(session.server.console_url()));
        }
        if theme::nav_item(ui, theme::Icon::Dashboard, t(l, "Live dashboard"), false).clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(format!("{}/app", session.server.address())));
        }
        let loading = session.loading();
        if ui
            .add_enabled_ui(!loading, |ui| theme::nav_item(ui, theme::Icon::Refresh, t(l, "Refresh"), false))
            .inner
            .clicked()
        {
            session.reload(ui.ctx());
        }
    }

    /// The windows above the page.
    fn dialogs(&mut self, ctx: &egui::Context, l: Lang) {
        if let Some(session) = self.session.as_mut() {
            dialogs::reveal(ctx, session, l, self.clipboard.as_mut());
            dialogs::revoke(ctx, session, l);
        }
        self.forget_dialogs(ctx, l);
        self.quit_dialog(ctx, l);
    }
}

/// The width (in points) from which the top bar has the language and the theme.
const TOP_BAR_ROOMY: f32 = 760.0;

/// The host of an address, as the top bar names the server.
fn host_of(address: &str) -> String {
    match address::normalize(address) {
        Ok(a) if a.class() == Class::Loopback => a.base().trim_start_matches("http://").to_owned(),
        Ok(a) => a.host,
        Err(_) => address.to_owned(),
    }
}

/// Whether the token crosses the network encrypted: always a word, never a
/// color alone, and the sentence that says what it means.
fn lock(l: Lang, address: &str) -> (Mark, &'static str, &'static str) {
    match address::normalize(address) {
        Ok(a) if a.scheme == address::Scheme::Https => {
            (Mark::Ok, t(l, "Encrypted"), t(l, "The connection is encrypted (HTTPS)."))
        }
        Ok(a) if a.class() == Class::Loopback => {
            (Mark::Off, t(l, "Local only"), t(l, "The connection is not encrypted, but it never leaves this computer."))
        }
        Ok(a) if a.class() == Class::Private => {
            (Mark::Warn, t(l, "Local network"), t(l, "The connection is not encrypted: it crosses your local network."))
        }
        _ => (
            Mark::Bad,
            t(l, "Not encrypted"),
            t(l, "The connection is not encrypted and crosses the Internet: the token can be read on the way."),
        ),
    }
}

fn theme_label(l: Lang, theme: Theme) -> &'static str {
    match theme {
        Theme::System => t(l, "Theme: as the system"),
        Theme::Light => t(l, "Theme: light"),
        Theme::Dark => t(l, "Theme: dark"),
    }
}

fn lang_label(l: Lang, lang: Option<Lang>) -> String {
    match lang {
        None => tf(l, "Language: as the system ({})", &[&lang_label(l, Some(Lang::of_system()))]),
        Some(Lang::En) => "English".into(),
        Some(Lang::Fr) => "Français".into(),
        Some(Lang::Es) => "Español".into(),
    }
}

/// A change came back.
fn took(ctx: &egui::Context, session: &mut Session, outcome: Outcome, l: Lang) {
    let notice = match outcome {
        Outcome::Settings(settings) => {
            let warning = settings.warning.is_some();
            session.take_settings(settings, l);
            if warning {
                Notice::warn(t(l, SAVED_NOT_BACKED_UP))
            } else {
                Notice::ok(t(l, "Saved: in force now, with no restart."))
            }
        }
        Outcome::Created(app, connection, lan) => {
            session.new_what.clear();
            session.new_name.clear();
            session.name_typed = false;
            session.new_error = None;
            session.unsure = None;
            let notice = tf(l, "Application {} created, with its own encrypted database.", &[&app.name]);
            session.reveal = Some(Reveal::new(app.name, Made::Created, false, connection, lan));
            Notice::ok(notice)
        }
        Outcome::Token { app, read_only, connection, lan } => {
            let notice = tf(l, "New token for {}.", &[&app]);
            session.token_asked = None;
            session.reveal = Some(Reveal::new(app, Made::Token, read_only, connection, lan));
            Notice::ok(notice)
        }
        Outcome::Revoked { app, id } => {
            // The token nobody has, once revoked, is no longer said.
            if session.token_asked.as_ref().is_some_and(|a| a.app == app && !a.known.contains(&id)) {
                session.token_asked = None;
            }
            Notice::ok(tf(l, "Token {}… of {} revoked: it opens nothing any more.", &[&id, &app]))
        }
        Outcome::Memory(app) => Notice::ok(tf(l, "Memory of {} saved: in force now.", &[&app.name])),
        Outcome::Origins(app) => {
            session.origin_field.clear();
            session.origin_error = None;
            let text = if app.origins.is_empty() {
                tf(l, "No website may call {} from a page any more.", &[&app.name])
            } else {
                tf(l, "Saved: pages of {} may call {} now.", &[&app.origins.join(", "), &app.name])
            };
            if let Some(apps) = session.apps.as_mut()
                && let Some(known) = apps.iter_mut().find(|a| a.name == app.name)
            {
                *known = app;
            }
            Notice::ok(text)
        }
    };
    session.notice = Some(notice);
    session.reload(ctx);
}

/// The answer of a change was lost on the way (a timeout, a reset, a page of
/// the host): it may or may not have been made. What is in force is read
/// again, and a secret that may have been made without reaching the editor
/// (a new application's token, a new token) is said.
fn unknown_change(ctx: &egui::Context, session: &mut Session, doing: Doing, e: &ApiError, l: Lang) {
    let timeout = matches!(e, ApiError::Timeout { .. });
    let text = match doing {
        Doing::Create => {
            session.unsure = Some(session.new_name.trim().to_owned());
            if timeout {
                t(l, "The server did not answer in time. The application may have been created anyway.").to_owned()
            } else {
                format!("{} {}", e.explain(l), t(l, "The application may have been created anyway: the list is read again."))
            }
        }
        _ if timeout => t(
            l,
            "The server did not answer in time: the change may or may not have been made. The page is read again to show what is in force.",
        )
        .to_owned(),
        _ => format!(
            "{} {}",
            e.explain(l),
            t(l, "The change may or may not have been made: the page is read again to show what is in force.")
        ),
    };
    if doing == Doing::Token
        && let Some(asked) = session.token_asked.as_mut()
    {
        asked.lost = true;
        asked.checked = false;
    }
    if doing == Doing::Settings {
        session.public_as_saved();
    }
    session.notice = Some(Notice::bad(text));
    session.reload(ctx);
}

/// A change was refused: said where it was asked.
fn failed_change(ctx: &egui::Context, session: &mut Session, doing: Doing, e: &ApiError, l: Lang) {
    let said = e.explain(l);
    match doing {
        // Refused: nothing was made.
        Doing::Token => session.token_asked = None,
        // The box shows what the server has, not what was asked.
        Doing::Settings => session.public_as_saved(),
        _ => {}
    }
    match (doing, e) {
        (Doing::Create, ApiError::Conflict(_)) => {
            session.new_error = Some(tf(
                l,
                "There is already an application named {}: choose another name.",
                &[&session.new_name.trim()],
            ));
        }
        (Doing::Create, ApiError::Invalid(detail)) => {
            session.new_error = Some(format!("{} {}", t(l, "Not created:"), detail_from_server(l, detail)));
        }
        (Doing::Origins, ApiError::Unsupported) => {
            session.origin_error = Some(t(l, OLDER_WEBSITES).to_owned());
        }
        (Doing::Origins, ApiError::Invalid(detail)) => {
            // The server says it in the person's language.
            session.origin_error = Some(format!("{} {detail}", t(l, "Not saved:")));
        }
        (Doing::Revoke, ApiError::NotFound(_)) => {
            session.notice = Some(Notice::warn(t(l, "This token was already revoked.")));
        }
        (Doing::Token | Doing::Memory | Doing::Origins, ApiError::NotFound(_)) => {
            session.notice = Some(Notice::bad(tf(
                l,
                "There is no application {} any more (deleted from the console?).",
                &[&session.open_app.clone().unwrap_or_default()],
            )));
        }
        _ => session.notice = Some(Notice::bad(said)),
    }
    if matches!(e, ApiError::NotFound(_)) {
        // What is there now.
        session.reload(ctx);
    }
}

/// A server's message, which is in English unless it is a refusal of the
/// settings: said as such.
fn detail_from_server(l: Lang, detail: &str) -> String {
    if l == Lang::En || detail.is_empty() {
        detail.to_owned()
    } else {
        tf(l, "Detail from the server (in English): {}", &[&detail])
    }
}

/// Said when a saved change is in force but not yet in the backup.
const SAVED_NOT_BACKED_UP: &str =
    "Saved and in force, but not yet in the backup: a restart of the server now would bring the earlier value back.";

/// Said where the websites are changed, on a server older than that route.
const OLDER_WEBSITES: &str = "This version of Nuage DB changes the websites in the web console only. The console asks for its password (NUAGE_ADMIN_PASSWORD).";

/// The page of a section of a connected server.
fn session_page(ui: &mut egui::Ui, session: &mut Session, here: &Here, l: Lang) {
    ui.add_space(8.0);
    if session.open_app.is_none() || session.section != Section::Applications {
        theme::page_title(ui, session.section.label(l), session.section.about(l));
    }
    notices(ui, session, l);
    ui.add_space(6.0);
    match session.section {
        Section::Overview => overview::page(ui, session, here, l),
        Section::Applications => match session.open_app.clone() {
            Some(name) => apps::app_page(ui, session, &name, l),
            None => apps::list_page(ui, session, l),
        },
        Section::Memory => settings::memory(ui, session, l),
        Section::Limits => settings::limits(ui, session, l),
        Section::Startup => settings::startup(ui, session, l),
        Section::Computer | Section::Tools | Section::About => {}
    }
}

/// What the page says above it: the last change's answer, a change on its
/// way, a refresh that failed.
fn notices(ui: &mut egui::Ui, session: &mut Session, l: Lang) {
    if let Some(n) = &session.notice {
        notice(ui, n);
    }
    if let Some((doing, action)) = &session.action {
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            let what = match doing {
                Doing::Create => t(l, "Creating…"),
                _ => t(l, "Saving…"),
            };
            ui.label(tf(
                l,
                "{} ({} s; the editor waits up to {} s)",
                &[&what, &action.elapsed().as_secs(), &session.server.timeout().as_secs()],
            ));
        });
    }
    if let Some(e) = session.stale.clone() {
        let mut again = false;
        theme::card(ui).show(ui, |ui| {
            theme::status(ui, Mark::Bad, tf(l, "Could not refresh: {}", &[&e.explain(l)]));
            if let Some((_, at)) = session.updated {
                ui.label(RichText::new(tf(l, "What is shown is as of {}.", &[&format::clock(at)])).weak());
            }
            again = ui.button(t(l, "Try again")).clicked();
        });
        if again {
            session.stale = None;
            session.reload(ui.ctx());
        }
    }
}

/// A message in a card, with its mark.
fn notice(ui: &mut egui::Ui, n: &Notice) {
    let p = theme::of(ui);
    let color = n.mark.color(p);
    egui::Frame::new().fill(p.card).stroke(egui::Stroke::new(1.0, color)).corner_radius(8).inner_margin(10).show(
        ui,
        |ui| {
            ui.horizontal_wrapped(|ui| {
                theme::mark(ui, n.mark);
                ui.label(RichText::new(&n.text).color(color));
            });
        },
    );
}

/// A line of help, in the muted color.
fn help(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).weak());
}

/// Why the settings file of this computer was not read.
fn local_error(l: Lang, e: local::ReadError) -> String {
    use local::ReadError as E;
    match e {
        E::NotInstalled => t(l, "No server was installed on this computer by the installer.").to_owned(),
        E::NeedsAdministrator(_) => t(l, "Its settings file is for administrators only.").to_owned(),
        E::Dismissed => t(l, "You cancelled: nothing was read. Try again, or paste the token yourself.").to_owned(),
        E::NotAccepted(why) => tf(
            l,
            "The password was not accepted, or this account cannot act as administrator ({}). If someone else installed Nuage DB, ask them to press Continue once in your session.",
            &[&why],
        ),
        E::NoPrompt(why) => tf(
            l,
            "This desktop cannot show a password window ({}): paste the token instead; it is the line NUAGE_ADMIN_TOKEN= of the settings file.",
            &[&why],
        ),
        E::Unreadable(why) => tf(l, "The settings file could not be read: {}.", &[&why.trim_end_matches('.')]),
        E::NotHandedOver(Some(code)) => tf(
            l,
            "Windows did not hand the file over (code {}). Try again; if it fails again, paste the token.",
            &[&code],
        ),
        E::NotHandedOver(None) => {
            t(l, "Windows did not hand the file over. Try again; if it fails again, paste the token.").to_owned()
        }
        E::Failed(why) => tf(l, "Its settings file could not be read: {}", &[&why]),
    }
}

/// The settings file `file` has no management token: what is true, and what to do.
fn no_token(l: Lang, file: &std::path::Path) -> String {
    tf(
        l,
        "This server has no management token: its settings file ({}) has no line NUAGE_ADMIN_TOKEN= (Linux servers installed before version 0.4.2). An administrator adds one, with a token from Tools, Generate a token, then restarts the service.",
        &[&file.display()],
    )
}

/// Said before the system asks for an administrator: what will run with
/// those rights, what it reads, and that the encryption key is not read.
const ASK_LINUX: &str = "Nuage DB keeps its settings in a file only administrators can read (/etc/nuage/nuage.env): it holds the encryption key. Your computer will now ask for your password.";
const ASK_MACOS: &str = "Nuage DB keeps its settings in a file only administrators can read (/usr/local/etc/nuage/nuage.env): it holds the encryption key. This Mac will now ask for an administrator's name and password.";
const ASK_WINDOWS: &str = "Nuage DB keeps its settings in a file only administrators can read (C:\\ProgramData\\Nuage\\nuage.env). Windows will now ask for permission. Its window names the Windows Command Processor, published by Microsoft: that is expected, the editor uses it to read four lines of this file, and changes nothing.";
const ASK_ADMINISTRATOR: &str = if cfg!(windows) {
    ASK_WINDOWS
} else if cfg!(target_os = "macos") {
    ASK_MACOS
} else {
    ASK_LINUX
};

/// Said after a secret was copied, the private way.
const COPIED_LINUX: &str = "Copied. It stays in the clipboard until you copy something else; the clipboard history is asked to skip it. On Linux, what you copy may disappear when the editor closes: paste it first.";
const COPIED_ELSEWHERE: &str =
    "Copied. It stays in the clipboard until you copy something else; the clipboard history is asked to skip it.";
const COPIED: &str = if cfg!(all(unix, not(target_os = "macos"))) { COPIED_LINUX } else { COPIED_ELSEWHERE };

/// The texts kept in constants (the source scan of the tests sees only literals).
#[cfg(test)]
pub(crate) const CONSTANT_TEXTS: &[&str] = &[
    ASK_LINUX,
    ASK_MACOS,
    ASK_WINDOWS,
    COPIED_LINUX,
    COPIED_ELSEWHERE,
    SAVED_NOT_BACKED_UP,
    OLDER_WEBSITES,
    settings::NO_SETTINGS,
    start::ASK_LINUX,
    start::ASK_MACOS,
    start::ASK_WINDOWS,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_said_of_the_settings_file_is_true() {
        let file = std::path::Path::new("/etc/nuage/nuage.env");
        for l in Lang::ALL {
            let said = no_token(l, file);
            assert!(said.contains("/etc/nuage/nuage.env") && said.contains("NUAGE_ADMIN_TOKEN="), "{said}");
            // Running the installer again adds no management token (see local::parity).
            for wrong in ["installer", "installateur", "instalador"] {
                assert!(
                    !said.contains(&format!("the {wrong} adds")) && !said.contains("l'installateur en ajoute"),
                    "{said}"
                );
            }
            for text in CONSTANT_TEXTS {
                assert!(!t(l, text).is_empty());
            }
            use local::ReadError as E;
            for e in [
                E::NotInstalled,
                E::NeedsAdministrator(file.to_path_buf()),
                E::Dismissed,
                E::NotAccepted("Not authorized".into()),
                E::NoPrompt("No authentication agent found.".into()),
                E::Unreadable("Permission denied".into()),
                E::NotHandedOver(Some(2)),
                E::NotHandedOver(None),
                E::Failed("x".into()),
            ] {
                assert!(!local_error(l, e.clone()).is_empty(), "{e:?}");
            }
        }
        // Each answer of the system has its own sentence, and a dismissal is never a refusal.
        let dismissed = local_error(Lang::En, local::ReadError::Dismissed);
        let refused = local_error(Lang::En, local::ReadError::NotAccepted("Not authorized".into()));
        let alone = local_error(Lang::En, local::ReadError::NoPrompt("no agent".into()));
        assert!(dismissed != refused && refused != alone && alone != dismissed);
        assert!(!alone.contains("cancelled") && !refused.contains("cancelled"));
    }

    #[test]
    fn the_lock_is_a_word() {
        assert_eq!(lock(Lang::En, "https://n.example.com").0, Mark::Ok);
        assert_eq!(lock(Lang::En, "http://127.0.0.1:8080").0, Mark::Off);
        assert_eq!(lock(Lang::En, "http://192.168.1.5:8080").0, Mark::Warn);
        assert_eq!(lock(Lang::Fr, "http://n.example.com").1, "Non chiffré");
        assert_eq!(lock(Lang::Fr, "http://127.0.0.1:8080").1, "Local seulement");
        assert_eq!(host_of("https://n.example.com"), "n.example.com");
        assert_eq!(host_of("http://127.0.0.1:8080"), "127.0.0.1:8080");
    }
}
