//! What the editor asks a Nuage DB server, over its management API
//! (`/api/apps`, `/api/settings`), with its management token
//! (`NUAGE_ADMIN_TOKEN`). Every call is blocking: the window runs them on a
//! thread of their own (see [`crate::jobs`]), never on its own.
//!
//! A redirect is never followed: the token would go to an address nobody
//! approved. Servers deployed before a route existed are told apart from
//! servers that refuse: the editor says what to update, and works with the
//! rest.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use zeroize::Zeroizing;

use crate::address::{self, Address, Class, Scheme};
use crate::i18n::{Lang, t, tf};
use crate::secret::Secret;

/// The shortest management token a server accepts (`nuage::server::MIN_ADMIN_TOKEN`).
pub const MIN_ADMIN_TOKEN: usize = 32;

/// How long calls may take, and how much they may bring back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transport {
    /// Reaching a server elsewhere.
    pub connect: Duration,
    /// Reaching a server of this computer: it answers at once, or it is not running.
    pub connect_loopback: Duration,
    /// A whole call. Saving a setting sends it to the backup at once, and
    /// Heroku's router gives up after 30 s anyway, with a page of its own
    /// (a change may have been made all the same).
    pub global: Duration,
    /// The largest answer read (answers are a few KB).
    pub body_limit: u64,
}

impl Default for Transport {
    fn default() -> Transport {
        Transport {
            connect: Duration::from_secs(10),
            // Windows answers a closed port of this computer only after about two
            // seconds (it tries the connection again first): past that, "not running".
            connect_loopback: Duration::from_secs(if cfg!(windows) { 5 } else { 2 }),
            global: Duration::from_secs(45),
            body_limit: 1 << 20,
        }
    }
}

/// What a server has, besides applications and tokens (0.4.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// `/api/settings` (0.4.7): memory, limits, what it was started with.
    pub settings: bool,
    /// `/api/status` opened by the management token, with the version.
    pub status: bool,
    /// `POST /api/apps/{name}/origins` (it came with the status above).
    pub origins: bool,
    /// The server's version, when its status says it.
    pub version: Option<String>,
}

/// The routes, for what their refusals mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// `/api/health`, called without the token.
    Health,
    /// `/api/apps`: the token, and a server of 0.4.2 or later.
    Apps,
    /// `/api/settings`.
    Settings,
    /// `/api/status`.
    Status,
    /// An application, its tokens, its memory, its websites.
    App,
    /// `/v1/ping`, with an application's token.
    Ping,
}

/// The steps of a connection, as the window shows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Step {
    /// `/api/health`, without the token.
    Looking = 0,
    /// `/api/apps`, with it.
    Token = 1,
    /// The settings and the status.
    Reading = 2,
}

impl Step {
    /// The step a counter says (see [`Server::connect_reporting`]).
    pub fn of(n: u8) -> Step {
        match n {
            0 => Step::Looking,
            1 => Step::Token,
            _ => Step::Reading,
        }
    }
}

/// Why a call failed, as the window says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// The address cannot be called (what was typed).
    BadAddress(String),
    /// No computer has this name (the host).
    HostNotFound(String),
    /// Nothing listens there.
    Refused { address: String, loopback: bool, port: u16 },
    /// No answer in time.
    Timeout { address: String, after: Duration },
    /// The secure connection failed (the system's words).
    Tls(String),
    /// The network failed in another way (the system's words).
    Network(String),
    /// An answer larger than any of Nuage DB's.
    TooLarge,
    /// Too many calls still wait for an answer.
    Busy,
    /// The editor failed (a bug): its message.
    Bug(String),
    /// A `3xx`: the editor does not follow it with the token. `upgrade`: the
    /// same host over `https://`, the one case offered as is.
    Redirect { from: String, to: String, upgrade: Option<String> },
    /// The address answers, but not as Nuage DB (a website, another program).
    NotNuage(String),
    /// A page of the host instead of the server's answer (the status).
    HostAnswered(u16),
    /// `401`: not the management token.
    TokenRefused,
    /// `403` on `/api/apps`: NUAGE_ADMIN_TOKEN is not set on the server.
    ManagementOff,
    /// `403` on the settings: they come from the environment only.
    ReadOnly(String),
    /// `/api/apps` is missing: a server before 0.4.2.
    TooOld,
    /// The route is missing: a server older than what was asked.
    Unsupported,
    /// No such application or token (the server's message).
    NotFound(String),
    /// It is there already (the server's message).
    Conflict(String),
    /// A value that does not fit: the server's message says why, with the figures.
    Invalid(String),
    /// The server failed: its status and message.
    Server(u16, String),
    /// No token is kept for this server any more.
    NotKept,
    /// The password store did not give the kept token (its words).
    Keystore(String),
}

impl ApiError {
    /// The server itself refused, in its own words (its JSON answer): what it
    /// has and does is known. Else the network, the host or the time failed
    /// on the way, and the call says nothing about the server.
    pub fn from_the_server(&self) -> bool {
        matches!(
            self,
            ApiError::TokenRefused
                | ApiError::ManagementOff
                | ApiError::ReadOnly(_)
                | ApiError::TooOld
                | ApiError::Unsupported
                | ApiError::NotFound(_)
                | ApiError::Conflict(_)
                | ApiError::Invalid(_)
        )
    }

    /// A change sent with this answer may or may not have been made: the
    /// answer was lost on the way (a timeout, a reset, a page of the host
    /// such as Heroku's after 30 s), or the server failed while making it.
    /// The other failures are a refusal, or the call never left.
    pub fn change_unknown(&self) -> bool {
        match self {
            ApiError::Timeout { .. }
            | ApiError::Network(_)
            | ApiError::NotNuage(_)
            | ApiError::TooLarge
            | ApiError::Bug(_) => true,
            ApiError::HostAnswered(status) | ApiError::Server(status, _) => *status >= 500,
            _ => false,
        }
    }

    /// What it means for the person, and what they can do, in `l`.
    pub fn explain(&self, l: Lang) -> String {
        let with = |what: &str, detail: &str| {
            if detail.is_empty() { what.to_owned() } else { format!("{what} ({detail})") }
        };
        match self {
            ApiError::BadAddress(typed) => {
                tf(l, "\"{}\" is not an address. Example: https://my-nuage.herokuapp.com", &[typed])
            }
            ApiError::HostNotFound(host) => tf(
                l,
                "No computer has the name {}: check the spelling, and that this computer is on the Internet.",
                &[host],
            ),
            ApiError::Refused { loopback: true, port, .. } => tf(
                l,
                "Nuage DB does not answer on this computer (port {}): it is stopped, or still starting.",
                &[port],
            ),
            ApiError::Refused { address, .. } => {
                tf(l, "Nothing answers at {}: is the server running, and is the port right?", &[address])
            }
            ApiError::Timeout { address, after } => tf(
                l,
                "{} did not answer within {}; a server that was asleep can take half a minute to wake. Try again.",
                &[address, &seconds(*after)],
            ),
            ApiError::Tls(detail) => with(
                t(
                    l,
                    "The secure connection (https) failed: the server's certificate is not trusted, or nothing speaks https at this address (a server of your own network may need http://). A network that inspects HTTPS causes this too. The editor never ignores a certificate.",
                ),
                detail,
            ),
            ApiError::Network(detail) => {
                with(t(l, "The connection failed: check this computer's network, then try again."), detail)
            }
            ApiError::TooLarge => {
                t(l, "The answer is larger than any of Nuage DB's (1 MiB): is this address really your Nuage DB server?")
                    .to_owned()
            }
            ApiError::Busy => t(l, "Too many calls are still waiting for an answer: try again in a moment.").to_owned(),
            ApiError::Bug(detail) => {
                with(t(l, "The editor failed unexpectedly (a bug). Try again; if it happens again, please report it."), detail)
            }
            ApiError::Redirect { from, to, upgrade: Some(_) } => tf(
                l,
                "{} sends the editor to {}, its https:// address: use that one (the editor does not follow it with your token by itself).",
                &[from, to],
            ),
            ApiError::Redirect { from, to, upgrade: None } => tf(
                l,
                "{} sends the editor to {}. The editor does not follow it with your token: if that is your server, type its address.",
                &[from, to],
            ),
            ApiError::NotNuage(address) => tf(
                l,
                "{} answers, but not as a Nuage DB server. Is it your website's address rather than Nuage DB's?",
                &[address],
            ),
            ApiError::HostAnswered(status) if *status >= 500 => tf(
                l,
                "The host answered {}: the server is starting, asleep or crashed. Try again in a minute (Heroku: More, then View logs).",
                &[status],
            ),
            ApiError::HostAnswered(status) => tf(
                l,
                "The host answered {} with a page of its own, not Nuage DB's answer: something between this computer and the server (a proxy) refuses the call.",
                &[status],
            ),
            ApiError::TokenRefused => t(
                l,
                "The server refused this token. The editor needs the management token (NUAGE_ADMIN_TOKEN), not an application's token nor the console password.",
            )
            .to_owned(),
            ApiError::ManagementOff => t(
                l,
                "This server's management API is off: NUAGE_ADMIN_TOKEN is not set on it. Tools, Generate a token makes one. Then set it: on Heroku in Settings, Config Vars (Heroku restarts the server by itself); with Docker in the .env file, then restart the container; for a server installed with the installer, in its settings file, then restart its service.",
            )
            .to_owned(),
            ApiError::ReadOnly(detail) => with(
                t(
                    l,
                    "This server takes its settings from its environment only (NUAGE_SETTINGS=environment): nothing is changed from here.",
                ),
                detail,
            ),
            ApiError::TooOld => {
                t(l, "This server is older than Nuage DB 0.4.2: update it to manage it from here.").to_owned()
            }
            ApiError::Unsupported => t(l, "This server's version cannot do this yet: update the server to do it here.").to_owned(),
            ApiError::NotFound(detail) => with(t(l, "Not found:"), detail),
            ApiError::Conflict(detail) | ApiError::Invalid(detail) => with(t(l, "Not saved:"), detail),
            ApiError::Server(status, detail) => with(&tf(l, "The server failed (HTTP {}).", &[status]), detail),
            ApiError::NotKept => t(l, "No token is kept for this server any more: type it again.").to_owned(),
            ApiError::Keystore(detail) => {
                with(t(l, "The password store did not give the kept token: type it instead."), detail)
            }
        }
    }
}

/// A duration as a sentence says it: `45 s`, `300 ms`.
fn seconds(d: Duration) -> String {
    if d >= Duration::from_secs(1) { format!("{} s", d.as_secs()) } else { format!("{} ms", d.as_millis()) }
}

/// An amount of memory an owner sets: a number of MiB, or `auto` (`None`).
pub type Amount = Option<u64>;

fn amount<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Amount, D::Error> {
    Ok(match Json::deserialize(d)? {
        Json::Number(n) => n.as_u64(),
        _ => None,
    })
}

/// An amount of memory as a person writes it, in MiB: a number alone
/// (`512`), or with its unit, in MiB (`512 MiB`, `512M`, `512 Mio`) or in GiB
/// (`2 GiB`, `2G`, `1.5 Go`, `1,5 Gio`); `None` for anything else (`auto`).
/// The server reads amounts the same way.
pub fn mib_of(typed: &str) -> Option<u64> {
    let s = typed.trim().to_ascii_lowercase().replace(',', ".");
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (number, unit) = s.split_at(end);
    match unit.trim() {
        "" | "m" | "mi" | "mib" | "mb" | "mo" | "mio" => number.parse::<u64>().ok(),
        "g" | "gi" | "gib" | "gb" | "go" | "gio" => {
            number.parse::<f64>().ok().filter(|g| g.is_finite() && *g < 1e9).map(|g| (g * 1024.0).round() as u64)
        }
        _ => None,
    }
}

// The answers are read leniently: a field a server does not send takes its
// default, and fields it adds are ignored, so older and newer servers parse.

/// A setting, as `/api/settings` gives it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Setting {
    /// Its name (`memory.cache_mb`).
    pub key: String,
    /// The variable of the environment it starts from (`NUAGE_CACHE_MB`).
    pub variable: String,
    /// The value in force (`None`: the default).
    pub value: Option<String>,
    /// `saved`, `environment` or `default`.
    pub source: String,
    /// What was saved.
    pub saved: Option<String>,
    /// What the environment, or the settings file, says.
    pub environment: Option<String>,
    /// What it is when nobody says anything.
    pub default: String,
}

impl Setting {
    /// The value in force, as a field shows it.
    pub fn shown(&self) -> String {
        self.value.clone().unwrap_or_else(|| self.default.clone())
    }
}

/// The memory of the server, in bytes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Memory {
    /// What the server counts on.
    pub total_bytes: Option<u64>,
    /// What the system reports.
    pub detected_bytes: Option<u64>,
    /// What is in use.
    pub used_bytes: Option<u64>,
    /// The budget of the page caches, all the applications together.
    pub cache_budget_bytes: Option<u64>,
    /// The most that budget may grow to.
    pub cache_ceiling_bytes: Option<u64>,
}

/// The memory of one application, as `/api/settings` gives it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppMemory {
    pub name: String,
    /// Set apart for its page cache (`None`: it shares).
    #[serde(deserialize_with = "amount")]
    pub cache_mb: Amount,
    /// Its key-value store (`None`: the server's rule).
    #[serde(deserialize_with = "amount")]
    pub kv_mb: Amount,
    /// What its page cache holds now.
    pub cache_bytes: u64,
    /// What it may hold.
    pub cache_capacity_bytes: u64,
}

/// A setting read once, when the server starts.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Startup {
    pub variable: String,
    /// `***` for a secret that is set; `None` when nothing was said.
    pub value: Option<String>,
}

/// Everything `/api/settings` answers.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    /// The server takes its settings from its environment only.
    pub read_only: bool,
    /// What was saved and is not in force (`read_only`).
    pub ignored: BTreeMap<String, String>,
    pub settings: Vec<Setting>,
    pub memory: Memory,
    pub applications: Vec<AppMemory>,
    pub startup: Vec<Startup>,
    /// The settings file the server was started with.
    pub settings_file: Option<String>,
    /// Saved and in force, but not yet in the backup.
    pub warning: Option<String>,
}

impl Settings {
    /// The setting named `key`.
    pub fn get(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key == key)
    }
}

/// A token of an application: never the token itself.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Token {
    /// Its first characters, shown to tell tokens apart.
    pub id: String,
    pub created_at: i64,
    pub read_only: bool,
}

/// The memory of an application, as `/api/apps` gives it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppAmounts {
    #[serde(deserialize_with = "amount")]
    pub cache_mb: Amount,
    #[serde(deserialize_with = "amount")]
    pub kv_mb: Amount,
}

/// An application of the server.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Application {
    pub name: String,
    pub description: String,
    /// The server's own application (`NUAGE_NAME`).
    pub first: bool,
    pub created_at: i64,
    pub tokens: Vec<Token>,
    /// `NUAGE_TOKEN` opens it too.
    pub env_token: bool,
    /// Websites whose pages may call it.
    pub origins: Vec<String>,
    pub memory: AppAmounts,
}

/// The memory of the server, as its status says it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StatusMemory {
    /// What the server counts on (the system's, or what was set).
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    /// What Nuage DB itself uses.
    pub process_bytes: Option<u64>,
}

/// Where the backups go, and how the last copy went.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Backup {
    pub enabled: bool,
    /// A folder or a bucket.
    pub target: Option<String>,
    /// The last copy of the application furthest behind (RFC 3339).
    pub last_at: Option<String>,
    pub last_ok: bool,
    /// The first failure, `application: what failed`.
    pub last_error: Option<String>,
    /// The second copy the owner chose, when the token sees the whole server.
    pub second: Option<SecondCopy>,
}

/// The second copy of the backups (la copie de secours): a complete copy of
/// every database, every hour, in a place of the owner's choice.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SecondCopy {
    pub target: String,
    /// The last complete copy (RFC 3339).
    pub last_at: Option<String>,
    pub last_error: Option<String>,
    pub running: bool,
}

/// What encrypts the data.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Encryption {
    pub enabled: bool,
    pub scheme: Option<String>,
}

/// The last client of an application.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LastConnection {
    /// RFC 3339.
    pub at: String,
    /// `HTTP API`, `SQL over HTTP`, `PostgreSQL`.
    pub via: String,
    /// Its User-Agent, a website's origin, or a PostgreSQL driver's name.
    pub client: String,
    pub read_only: bool,
}

impl LastConnection {
    /// It was the editor testing a token (its User-Agent), not a website or a program.
    pub fn by_the_editor(&self) -> bool {
        self.client.starts_with("nuage-editor/")
    }
}

/// An application, as the status lists it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DbStatus {
    pub name: String,
    /// `ok`, or `degraded` when its backup fails.
    pub status: String,
    pub tables: u64,
    pub rows: u64,
    pub bytes: u64,
    pub websites: Vec<String>,
    pub last_connection: Option<LastConnection>,
}

/// What `/api/status` says, opened by the management token (S1): the
/// version, the memory, the backups, every application.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Status {
    pub version: String,
    pub uptime_s: u64,
    pub memory: StatusMemory,
    pub backup: Backup,
    pub encryption: Encryption,
    pub databases: Vec<DbStatus>,
}

impl Status {
    /// The application named `name`.
    pub fn database(&self, name: &str) -> Option<&DbStatus> {
        self.databases.iter().find(|d| d.name == name)
    }
}

/// What `/v1/ping` says of an application's token.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Ping {
    pub version: String,
    pub application: String,
    /// `read-write` or `read-only`.
    pub access: String,
    pub branch: String,
    pub tables: u64,
    /// How long the call took.
    #[serde(skip)]
    pub millis: u128,
}

impl Ping {
    pub fn read_only(&self) -> bool {
        self.access == "read-only"
    }
}

/// What an application needs to connect.
#[derive(Debug, PartialEq, Eq)]
pub struct Connection {
    /// The token, shown once: the server keeps only its digest.
    pub token: Secret,
    /// `NUAGE_URL`: the server's public address when it has one, else the
    /// address the editor used.
    pub url: String,
}

impl Connection {
    /// The two lines an application's settings take.
    pub fn env_lines(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("NUAGE_URL={}\nNUAGE_TOKEN={}", self.url, self.token.expose()))
    }

    /// `NUAGE_URL` reaches this computer only (`127.0.0.1`, `localhost`).
    pub fn is_this_computer_only(&self) -> bool {
        address::normalize(&self.url).is_ok_and(|a| a.class() == Class::Loopback)
    }
}

/// A server once connected: what it has, and what it said.
#[derive(Debug)]
pub struct Connected {
    pub server: Server,
    pub apps: Vec<Application>,
    /// `None`: an older server without settings, or they failed (`settings_error`).
    pub settings: Option<Settings>,
    pub settings_error: Option<ApiError>,
    /// `None`: the management token does not open the status (an older
    /// server), or it failed on the way (`status_error`).
    pub status: Option<Status>,
    /// The network or the host failed while the status was asked: it is
    /// asked again at the next refresh.
    pub status_error: Option<ApiError>,
}

/// A Nuage DB server and its management token.
#[derive(Clone)]
pub struct Server {
    address: Address,
    base: String,
    token: Arc<Secret>,
    lang: Lang,
    agent: ureq::Agent,
    transport: Transport,
    /// The address its applications use, when it is not the one called
    /// (`SITE_URL` of a server of this computer).
    public: Option<String>,
    caps: Caps,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token prints as Secret(***).
        f.debug_struct("Server")
            .field("base", &self.base)
            .field("token", &self.token)
            .field("public", &self.public)
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

/// `text` as one segment of a path (an application's name, a token's id).
fn segment(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => char::from(b).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Post,
    Delete,
}

impl Server {
    /// The server at `address` (as typed: it is normalized), opened with
    /// `token`. Its refusals come back in `lang`.
    pub fn new(address: &str, token: Secret, lang: Lang, transport: &Transport) -> Result<Server, ApiError> {
        let address = address::normalize(address).map_err(|_| ApiError::BadAddress(address.trim().to_owned()))?;
        let connect = if address.class() == Class::Loopback { transport.connect_loopback } else { transport.connect };
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // A 3xx comes back as it is: the token goes nowhere else.
            .max_redirects(0)
            .timeout_connect(Some(connect))
            .timeout_global(Some(transport.global))
            .user_agent(concat!("nuage-editor/", env!("CARGO_PKG_VERSION")));
        // The proxy of the environment (HTTPS_PROXY, ALL_PROXY) is for the Internet: through it,
        // "this computer" and its network would be the proxy's, and the token would go there.
        if matches!(address.class(), Class::Loopback | Class::Private) {
            config = config.proxy(None);
        }
        let agent = config.build().into();
        let base = address.base();
        Ok(Server {
            address,
            base,
            token: Arc::new(token),
            lang,
            agent,
            transport: transport.clone(),
            public: None,
            caps: Caps::default(),
        })
    }

    /// The address its applications use (`SITE_URL`), when it is not the one called.
    pub fn with_public(mut self, public: Option<String>) -> Server {
        self.public = public.map(|p| p.trim().trim_end_matches('/').to_owned()).filter(|p| !p.is_empty());
        self
    }

    /// Its address.
    pub fn address(&self) -> &str {
        &self.base
    }

    /// The address the applications are given.
    pub fn public_address(&self) -> &str {
        self.public.as_deref().unwrap_or(&self.base)
    }

    /// How long a call may take.
    pub fn timeout(&self) -> Duration {
        self.transport.global
    }

    /// What it has (known once connected).
    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    /// What a later read of the status says of it: a status, with its
    /// version (`Some`), or the server's own no (`None`, an older server).
    pub fn learned_status(&mut self, version: Option<String>) {
        self.caps.status = version.is_some();
        self.caps.origins = self.caps.status;
        self.caps.version = version;
    }

    /// The token, for the one place that keeps it (the password store).
    pub fn token(&self) -> &Secret {
        &self.token
    }

    /// Its console, in a browser.
    pub fn console_url(&self) -> String {
        format!("{}/admin", self.base)
    }

    /// Checks the server step by step, and what it has: `/api/health` without
    /// the token (a wrong address, a website, a captive portal never gets it),
    /// `/api/apps` with it, then the settings and the status together.
    pub fn connect(self) -> Result<Connected, ApiError> {
        self.connect_reporting(&AtomicU8::new(0))
    }

    /// [`Server::connect`], saying in `step` which [`Step`] it is at.
    pub fn connect_reporting(mut self, step: &AtomicU8) -> Result<Connected, ApiError> {
        step.store(Step::Looking as u8, Ordering::SeqCst);
        self.health()?;
        step.store(Step::Token as u8, Ordering::SeqCst);
        let apps = self.applications()?;
        step.store(Step::Reading as u8, Ordering::SeqCst);
        let (settings, status) = std::thread::scope(|scope| {
            let this = &self;
            let status =
                std::thread::Builder::new().name("nuage-editor-call".into()).spawn_scoped(scope, || this.status());
            let settings = this.settings();
            let status = match status {
                Ok(handle) => handle.join().unwrap_or_else(|_| Err(ApiError::Bug("the status call stopped".into()))),
                // No thread: one after the other.
                Err(_) => this.status(),
            };
            (settings, status)
        });
        // An older server answers a token it does not know with its public
        // status, which has no version, or with 401: that is no status (S1 and
        // S2 came together). A failure on the way (a 503 page while it wakes, a
        // timeout) says nothing: the next refresh asks again.
        let (status, status_error) = match status {
            Ok(status) => (Some(status), None),
            Err(e) if e.from_the_server() => (None, None),
            Err(e) => (None, Some(e)),
        };
        self.caps.status = status.is_some() || status_error.is_some();
        self.caps.origins = self.caps.status;
        self.caps.version = status.as_ref().map(|s| s.version.clone());
        let (settings, settings_error) = match settings {
            Ok(s) => (Some(s), None),
            Err(ApiError::Unsupported) => (None, None),
            Err(e) => (None, Some(e)),
        };
        self.caps.settings = settings.is_some() || settings_error.is_some();
        Ok(Connected { server: self, apps, settings, settings_error, status, status_error })
    }

    /// One call with the management token (`bearer`), or without any.
    fn call(
        &self,
        route: Route,
        method: Method,
        path: &str,
        body: Option<Json>,
        bearer: bool,
    ) -> Result<Json, ApiError> {
        self.call_as(route, method, path, body, bearer.then_some(&*self.token))
    }

    /// One call: the answer's JSON, or why it failed. `token`: the one it
    /// carries (the management token, an application's), or none.
    fn call_as(
        &self,
        route: Route,
        method: Method,
        path: &str,
        body: Option<Json>,
        token: Option<&Secret>,
    ) -> Result<Json, ApiError> {
        let url = format!("{}{path}", self.base);
        let lang = self.lang.code();
        let auth = Zeroizing::new(token.map(|t| format!("Bearer {}", t.expose())).unwrap_or_default());
        let answer = match method {
            Method::Get => {
                let request = self.agent.get(&url).header("accept-language", lang);
                if token.is_some() { request.header("authorization", auth.as_str()).call() } else { request.call() }
            }
            Method::Delete => {
                self.agent.delete(&url).header("accept-language", lang).header("authorization", auth.as_str()).call()
            }
            Method::Post => self
                .agent
                .post(&url)
                .header("accept-language", lang)
                .header("authorization", auth.as_str())
                .send_json(body.unwrap_or_else(|| json!({}))),
        };
        let mut answer = answer.map_err(|e| self.failed(e, true))?;
        let status = answer.status().as_u16();
        let header = |name: &str| answer.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let location = header("location");
        let is_json =
            header("content-type").is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("application/json"));
        // The answer may hold a token: its bytes are wiped once read.
        let bytes = Zeroizing::new(
            answer
                .body_mut()
                .with_config()
                .limit(self.transport.body_limit)
                .read_to_vec()
                .map_err(|e| self.failed(e, false))?,
        );
        let json = if is_json { serde_json::from_slice::<Json>(&bytes).ok() } else { None };
        match (status, json) {
            (200..=299, Some(json)) => Ok(json),
            (status, json) => Err(refusal(route, &self.base, status, json.as_ref(), location.as_deref())),
        }
    }

    /// What a failure of the transport means. `connecting`: before the answer
    /// came (a broken TLS stream reads as invalid data then).
    fn failed(&self, e: ureq::Error, connecting: bool) -> ApiError {
        let https = self.address.scheme == Scheme::Https;
        match e {
            ureq::Error::HostNotFound => ApiError::HostNotFound(self.address.host.clone()),
            ureq::Error::Timeout(which) => ApiError::Timeout {
                address: self.base.clone(),
                after: match which {
                    ureq::Timeout::Connect | ureq::Timeout::Resolve if self.address.class() == Class::Loopback => {
                        self.transport.connect_loopback
                    }
                    ureq::Timeout::Connect | ureq::Timeout::Resolve => self.transport.connect,
                    _ => self.transport.global,
                },
            },
            // The system found no address for the name (ureq says HostNotFound only for an empty list).
            ureq::Error::Io(io) if lookup_failed(&io) => ApiError::HostNotFound(self.address.host.clone()),
            ureq::Error::Io(io) => match io.kind() {
                std::io::ErrorKind::ConnectionRefused => ApiError::Refused {
                    address: self.base.clone(),
                    loopback: self.address.class() == Class::Loopback,
                    port: self.address.port_or_default(),
                },
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                    ApiError::Timeout { address: self.base.clone(), after: self.transport.global }
                }
                std::io::ErrorKind::InvalidData if https && connecting => ApiError::Tls(io.to_string()),
                _ => ApiError::Network(io.to_string()),
            },
            ureq::Error::Rustls(e) => ApiError::Tls(e.to_string()),
            ureq::Error::Tls(e) => ApiError::Tls(e.to_owned()),
            ureq::Error::BodyExceedsLimit(_) | ureq::Error::LargeResponseHeader(..) => ApiError::TooLarge,
            ureq::Error::BadUri(_) => ApiError::BadAddress(self.base.clone()),
            // Not HTTP: another program listens there.
            ureq::Error::Protocol(_) => ApiError::NotNuage(self.base.clone()),
            ureq::Error::ConnectProxyFailed(why) => ApiError::Network(format!("proxy: {why}")),
            other => ApiError::Network(other.to_string()),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(&self, json: Json) -> Result<T, ApiError> {
        serde_json::from_value(json).map_err(|_| ApiError::NotNuage(self.base.clone()))
    }

    /// `/api/health`, without the token: `{"status":"ok"}` from Nuage DB.
    pub fn health(&self) -> Result<(), ApiError> {
        let json = self.call(Route::Health, Method::Get, "/api/health", None, false)?;
        match json.get("status").and_then(Json::as_str) {
            Some("ok") => Ok(()),
            _ => Err(ApiError::NotNuage(self.base.clone())),
        }
    }

    /// The live status. Only a whole one counts: the public subset an older
    /// server gives a token it does not know has no `version`.
    pub fn status(&self) -> Result<Status, ApiError> {
        let json = self.call(Route::Status, Method::Get, "/api/status", None, true)?;
        let status: Status = self.parse(json)?;
        if status.version.is_empty() { Err(ApiError::Unsupported) } else { Ok(status) }
    }

    /// Asks `/v1/ping` with `token`, an application's: which application it
    /// opens, and how. The server notes the editor as its last client.
    pub fn ping_with(&self, token: &Secret) -> Result<Ping, ApiError> {
        let started = std::time::Instant::now();
        let json = self.call_as(Route::Ping, Method::Get, "/v1/ping", None, Some(token))?;
        let mut ping: Ping = self.parse(json)?;
        if ping.application.is_empty() {
            return Err(ApiError::NotNuage(self.base.clone()));
        }
        ping.millis = started.elapsed().as_millis();
        Ok(ping)
    }

    /// [`Server::ping_with`] its own token (an application's, in Tools).
    pub fn ping(&self) -> Result<Ping, ApiError> {
        self.ping_with(&self.token)
    }

    /// The websites whose pages may call `name`, all of them at once (`[]`: none).
    pub fn set_origins(&self, name: &str, origins: &[String]) -> Result<Application, ApiError> {
        let path = format!("/api/apps/{}/origins", segment(name));
        self.parse(self.call(Route::App, Method::Post, &path, Some(json!({ "origins": origins })), true)?)
    }

    /// The settings, the memory and the applications' amounts (0.4.7).
    pub fn settings(&self) -> Result<Settings, ApiError> {
        self.parse(self.call(Route::Settings, Method::Get, "/api/settings", None, true)?)
    }

    /// Saves settings (`None`: back to the environment's value), all or none.
    pub fn save_settings(&self, changes: &[(&str, Option<String>)]) -> Result<Settings, ApiError> {
        let body: serde_json::Map<String, Json> =
            changes.iter().map(|(k, v)| ((*k).to_owned(), v.clone().map_or(Json::Null, Json::String))).collect();
        self.parse(self.call(Route::Settings, Method::Post, "/api/settings", Some(Json::Object(body)), true)?)
    }

    /// Gives a setting back to the environment's value, or the default.
    pub fn reset_setting(&self, key: &str) -> Result<Settings, ApiError> {
        let path = format!("/api/settings/{}", segment(key));
        self.parse(self.call(Route::Settings, Method::Delete, &path, None, true)?)
    }

    /// Every application.
    pub fn applications(&self) -> Result<Vec<Application>, ApiError> {
        let mut json = self.call(Route::Apps, Method::Get, "/api/apps", None, true)?;
        match json.get_mut("applications").map(Json::take) {
            Some(list) => self.parse(list),
            None => Err(ApiError::NotNuage(self.base.clone())),
        }
    }

    /// A new application, with its first token.
    pub fn create_application(&self, name: &str, description: &str) -> Result<(Application, Connection), ApiError> {
        let body = json!({ "name": name.trim(), "description": description });
        let mut json = self.call(Route::App, Method::Post, "/api/apps", Some(body), true)?;
        let app = self.parse(json.get_mut("application").map(Json::take).unwrap_or_default())?;
        Ok((app, self.connection_of(json)?))
    }

    /// Another token for `name`; `read_only`: it reads, never writes.
    pub fn new_token(&self, name: &str, read_only: bool) -> Result<Connection, ApiError> {
        let path = format!("/api/apps/{}/tokens", segment(name));
        let json = self.call(Route::App, Method::Post, &path, Some(json!({ "read_only": read_only })), true)?;
        self.connection_of(json)
    }

    /// Revokes the token `id` of `name`: it opens nothing any more.
    pub fn revoke_token(&self, name: &str, id: &str) -> Result<(), ApiError> {
        let path = format!("/api/apps/{}/tokens/{}", segment(name), segment(id));
        self.call(Route::App, Method::Delete, &path, None, true).map(|_| ())
    }

    /// The memory of one application: amounts as typed (`2 GiB`, `512`,
    /// `auto`); `None` leaves one as it is.
    pub fn set_memory(&self, name: &str, cache: Option<&str>, kv: Option<&str>) -> Result<Application, ApiError> {
        let mut body = serde_json::Map::new();
        for (key, value) in [("cache_mb", cache), ("kv_mb", kv)] {
            if let Some(v) = value {
                let v = v.trim();
                body.insert(key.into(), if v.is_empty() { json!("auto") } else { json!(v) });
            }
        }
        let path = format!("/api/apps/{}/memory", segment(name));
        self.parse(self.call(Route::App, Method::Post, &path, Some(Json::Object(body)), true)?)
    }

    /// The new token of an answer, taken out of it (the copy the server put
    /// in `env` is wiped), and the address the applications use.
    fn connection_of(&self, mut json: Json) -> Result<Connection, ApiError> {
        if let Some(Json::String(copy)) = json.pointer_mut("/env/NUAGE_TOKEN").map(Json::take) {
            drop(Zeroizing::new(copy));
        }
        match json.get_mut("token").map(Json::take) {
            Some(Json::String(token)) if !token.is_empty() => {
                Ok(Connection { token: Secret::new(token), url: self.public_address().to_owned() })
            }
            _ => Err(ApiError::NotNuage(self.base.clone())),
        }
    }
}

/// The system could not turn a name into an address: no such name, or no
/// answer from the name servers (this computer is offline). std says it
/// `failed to lookup address information: …` on Unix and macOS; Windows
/// gives the WSA code (HOST_NOT_FOUND, TRY_AGAIN, NO_RECOVERY, NO_DATA).
fn lookup_failed(io: &std::io::Error) -> bool {
    if cfg!(windows) {
        matches!(io.raw_os_error(), Some(11001..=11004))
    } else {
        io.raw_os_error().is_none() && io.to_string().starts_with("failed to lookup address information")
    }
}

/// What a refused answer means, by route: `status`, its JSON (`None`: not
/// JSON, a page), and its `Location`.
pub fn refusal(route: Route, base: &str, status: u16, json: Option<&Json>, location: Option<&str>) -> ApiError {
    let message = json.and_then(|j| j.get("error")?.get("message")?.as_str()).map(str::to_owned).unwrap_or_default();
    let no_route = message.is_empty() || message.starts_with("no such route") || message.starts_with("no such page");
    match status {
        300..=399 => match location {
            Some(to) => redirect(base, to),
            None => ApiError::HostAnswered(status),
        },
        // A 2xx that is not JSON, or not the JSON asked for.
        200..=299 => ApiError::NotNuage(base.to_owned()),
        _ if route == Route::Health => {
            if status >= 500 && json.is_none() {
                ApiError::HostAnswered(status)
            } else {
                ApiError::NotNuage(base.to_owned())
            }
        }
        // A page of the host, a proxy, a router: not the server's answer.
        _ if json.is_none() => ApiError::HostAnswered(status),
        401 => ApiError::TokenRefused,
        403 if route == Route::Settings => ApiError::ReadOnly(message),
        403 => ApiError::ManagementOff,
        404 if no_route && route == Route::Apps => ApiError::TooOld,
        404 if no_route => ApiError::Unsupported,
        405 => ApiError::Unsupported,
        404 => ApiError::NotFound(message),
        409 => ApiError::Conflict(message),
        400 | 413 | 422 => ApiError::Invalid(message),
        _ => ApiError::Server(status, message),
    }
}

/// A redirect from `base` to `location` (absolute, or a path of `base`).
fn redirect(base: &str, location: &str) -> ApiError {
    let location = location.trim();
    let lower = location.to_ascii_lowercase();
    let to = if lower.starts_with("http://") || lower.starts_with("https://") {
        location.to_owned()
    } else if let Some(rest) = location.strip_prefix("//") {
        let scheme = base.split_once("://").map_or("https", |(s, _)| s);
        format!("{scheme}://{rest}")
    } else if location.starts_with('/') {
        format!("{base}{location}")
    } else {
        format!("{base}/{location}")
    };
    // Only the same host going from http to https is offered as is.
    let upgrade = match (address::normalize(base), address::normalize(&to)) {
        (Ok(from), Ok(target))
            if from.scheme == Scheme::Http && target.scheme == Scheme::Https && from.host == target.host =>
        {
            Some(target.base())
        }
        _ => None,
    };
    ApiError::Redirect { from: base.to_owned(), to, upgrade }
}

/// What a typed management token is, once a pasted `NUAGE_ADMIN_TOKEN=…`
/// line, quotes and spaces are taken off; or why it cannot be one (said
/// before anything is sent).
pub fn typed_token(typed: &str) -> Result<Secret, TokenError> {
    let text = typed.trim();
    let text = text.strip_prefix("export ").map_or(text, str::trim_start);
    let text = text.strip_prefix("NUAGE_ADMIN_TOKEN").map_or(text, |rest| rest.trim_start().trim_start_matches('='));
    let text = text.trim().trim_matches(['"', '\'']).trim();
    if text.is_empty() {
        Err(TokenError::Empty)
    } else if text.starts_with("nuage_") {
        Err(TokenError::Application)
    } else if !text.chars().all(|c| c.is_ascii_graphic()) {
        Err(TokenError::Characters)
    } else if text.chars().count() < MIN_ADMIN_TOKEN / 2 {
        Err(TokenError::Short)
    } else if text.chars().count() < MIN_ADMIN_TOKEN {
        // The installer's console password has 24 characters.
        Err(TokenError::Password)
    } else {
        Ok(Secret::new(text.to_owned()))
    }
}

/// Whether `name` can be an application's name, as the server's engine
/// checks it: 1 to 63 lowercase letters, digits, `-` and `_`, not `main`.
pub fn app_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && name != "main"
}

/// A name for an application, from what it is for (`Ma Boutique Été` gives
/// `ma-boutique-ete`): lowercase, accents folded, any other character a `-`.
pub fn suggest_name(what: &str) -> String {
    let mut name = String::new();
    for c in what.chars().flat_map(char::to_lowercase) {
        let folded = match c {
            'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => "a",
            'ç' => "c",
            'é' | 'è' | 'ê' | 'ë' => "e",
            'í' | 'ì' | 'î' | 'ï' => "i",
            'ñ' => "n",
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => "o",
            'ú' | 'ù' | 'û' | 'ü' => "u",
            'ý' | 'ÿ' => "y",
            'œ' => "oe",
            'æ' => "ae",
            'ß' => "ss",
            c if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' => {
                name.push(c);
                continue;
            }
            _ => "-",
        };
        if folded == "-" && (name.is_empty() || name.ends_with('-')) {
            continue;
        }
        name.push_str(folded);
    }
    let mut name: String = name.trim_end_matches('-').chars().take(63).collect();
    while name.ends_with('-') {
        name.pop();
    }
    name
}

/// A website's origin as the server keeps it (`https://monsite.fr`), or
/// `None` when it is not one: a scheme and a host, without a path; `*` for any.
pub fn origin(typed: &str) -> Option<String> {
    let o = typed.trim().trim_end_matches('/').to_ascii_lowercase();
    if o == "*" {
        return Some(o);
    }
    let host = o.strip_prefix("https://").or_else(|| o.strip_prefix("http://"))?;
    let fits = !host.is_empty() && !host.chars().any(|c| c.is_whitespace() || matches!(c, '/' | ',' | '?' | '#' | '@'));
    fits.then_some(o)
}

/// Why a typed text is not a management token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenError {
    Empty,
    /// Under half of [`MIN_ADMIN_TOKEN`] characters.
    Short,
    /// Under [`MIN_ADMIN_TOKEN`] characters, as many as a password: the
    /// console's, perhaps.
    Password,
    /// `nuage_…`: an application's token.
    Application,
    /// Spaces or characters a token does not have.
    Characters,
}

impl TokenError {
    pub fn explain(self, l: Lang) -> String {
        match self {
            TokenError::Empty => t(l, "Type its management token first.").to_owned(),
            TokenError::Short => {
                tf(l, "A management token has {} characters at least: is part of it missing?", &[&MIN_ADMIN_TOKEN])
            }
            TokenError::Password => tf(
                l,
                "A management token has {} characters at least: is part of it missing, or is this the console password (NUAGE_ADMIN_PASSWORD)? The editor needs NUAGE_ADMIN_TOKEN, 64 characters when the installer or Heroku made it.",
                &[&MIN_ADMIN_TOKEN],
            ),
            TokenError::Application => {
                t(l, "This is an application's token (NUAGE_TOKEN), not the management token.").to_owned()
            }
            TokenError::Characters => {
                t(l, "A management token has letters, digits and signs only, without spaces.").to_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read_as_the_server_writes_them() {
        let settings: Settings = serde_json::from_value(json!({
            "read_only": false, "ignored": {}, "settings_file": null,
            "settings": [{ "key": "memory.cache_mb", "variable": "NUAGE_CACHE_MB", "value": null, "source": "default",
                           "saved": null, "environment": null, "default": "auto" }],
            "memory": { "total_bytes": 536870912u64, "detected_bytes": 536870912u64, "used_bytes": 1, "cache_budget_bytes": 2,
                        "cache_ceiling_bytes": 3 },
            "applications": [{ "name": "shop", "cache_mb": 64, "kv_mb": "auto", "cache_bytes": 0, "cache_capacity_bytes": 9 }],
            "startup": [{ "variable": "PORT", "value": "8080" }],
        }))
        .unwrap();
        assert_eq!(settings.get("memory.cache_mb").unwrap().shown(), "auto");
        assert_eq!((settings.applications[0].cache_mb, settings.applications[0].kv_mb), (Some(64), None));
        let app: Application = serde_json::from_value(json!({
            "name": "shop", "description": "", "first": false, "created_at": 1, "tokens": [], "env_token": false,
            "origins": [], "memory": { "cache_mb": "auto", "kv_mb": 8 },
        }))
        .unwrap();
        assert_eq!((app.memory.cache_mb, app.memory.kv_mb), (None, Some(8)));
        let connection = Connection { token: Secret::new("nuage_x".into()), url: "https://n.example".into() };
        assert_eq!(connection.env_lines().as_str(), "NUAGE_URL=https://n.example\nNUAGE_TOKEN=nuage_x");
        assert!(!format!("{connection:?}").contains("nuage_x"), "never printed");
        assert!(!connection.is_this_computer_only());
        let here = Connection { token: Secret::new("nuage_x".into()), url: "http://127.0.0.1:8080".into() };
        assert!(here.is_this_computer_only());
        assert_eq!(
            (mib_of("2 Go"), mib_of("1,5 Gio"), mib_of("512"), mib_of("auto")),
            (Some(2048), Some(1536), Some(512), None)
        );
    }

    #[test]
    fn older_and_newer_servers_both_parse() {
        // An application of 0.4.2 (no memory, no origins), and one of a later
        // version with fields this editor does not know.
        let old: Application = serde_json::from_value(json!({ "name": "shop", "tokens": [{ "id": "ab12" }] })).unwrap();
        assert_eq!((old.name.as_str(), old.tokens.len(), old.memory.cache_mb), ("shop", 1, None));
        let new: Application =
            serde_json::from_value(json!({ "name": "shop", "last_seen": 3, "memory": { "cache_mb": 8, "x": 1 } }))
                .unwrap();
        assert_eq!(new.memory.cache_mb, Some(8));
        let settings: Settings = serde_json::from_value(json!({ "settings": [{ "key": "k" }], "more": [] })).unwrap();
        assert_eq!(settings.get("k").unwrap().shown(), "");
    }

    #[test]
    fn refusals_by_route() {
        let base = "http://127.0.0.1:8080";
        let error = |message: &str| json!({ "error": { "kind": "x", "message": message } });
        let no_route = error("no such route: see /v1/openapi.json");
        let no_page = error("no such page: /api/settings");
        for (route, status, json, expected) in [
            (Route::Settings, 404, Some(&no_page), ApiError::Unsupported),
            (Route::Settings, 405, Some(&no_route), ApiError::Unsupported),
            (Route::Apps, 404, Some(&no_page), ApiError::TooOld),
            (Route::Apps, 403, Some(&error("off")), ApiError::ManagementOff),
            (Route::Settings, 403, Some(&error("environment")), ApiError::ReadOnly("environment".into())),
            (Route::Apps, 401, Some(&error("no")), ApiError::TokenRefused),
            (
                Route::App,
                404,
                Some(&error("there is no application `x`")),
                ApiError::NotFound("there is no application `x`".into()),
            ),
            (Route::App, 409, Some(&error("taken")), ApiError::Conflict("taken".into())),
            (Route::App, 400, Some(&error("bad name")), ApiError::Invalid("bad name".into())),
            (Route::App, 500, Some(&error("disk full")), ApiError::Server(500, "disk full".into())),
            (Route::Apps, 503, None, ApiError::HostAnswered(503)),
            (Route::Health, 503, None, ApiError::HostAnswered(503)),
            (Route::Health, 404, None, ApiError::NotNuage(base.into())),
            (Route::Health, 404, Some(&no_page), ApiError::NotNuage(base.into())),
            (Route::Apps, 200, None, ApiError::NotNuage(base.into())),
        ] {
            assert_eq!(refusal(route, base, status, json, None), expected, "{route:?} {status}");
        }
        assert_eq!(
            refusal(Route::Health, base, 301, None, Some("https://127.0.0.1:8080/api/health")),
            ApiError::Redirect {
                from: base.into(),
                to: "https://127.0.0.1:8080/api/health".into(),
                upgrade: Some("https://127.0.0.1:8080".into()),
            }
        );
        assert_eq!(
            refusal(Route::Apps, "https://n.example.com", 302, None, Some("/admin/login")),
            ApiError::Redirect {
                from: "https://n.example.com".into(),
                to: "https://n.example.com/admin/login".into(),
                upgrade: None
            }
        );
        let elsewhere = refusal(Route::Health, "http://n.example.com", 301, None, Some("https://other.example.com/"));
        assert!(matches!(elsewhere, ApiError::Redirect { upgrade: None, .. }), "{elsewhere:?}");
        assert_eq!(refusal(Route::Health, base, 304, None, None), ApiError::HostAnswered(304));
    }

    #[test]
    fn every_error_is_said_in_three_languages() {
        let all = [
            ApiError::BadAddress("x y".into()),
            ApiError::HostNotFound("nuage.example.com".into()),
            ApiError::Refused { address: "http://127.0.0.1:8080".into(), loopback: true, port: 8080 },
            ApiError::Refused { address: "http://192.168.1.2:8080".into(), loopback: false, port: 8080 },
            ApiError::Timeout { address: "https://n.example.com".into(), after: Duration::from_secs(45) },
            ApiError::Tls("invalid peer certificate: UnknownIssuer".into()),
            ApiError::Network("connection reset".into()),
            ApiError::TooLarge,
            ApiError::Busy,
            ApiError::Bug("index out of bounds".into()),
            ApiError::Redirect { from: "http://a".into(), to: "https://a".into(), upgrade: Some("https://a".into()) },
            ApiError::Redirect { from: "http://a".into(), to: "https://b".into(), upgrade: None },
            ApiError::NotNuage("https://n.example.com".into()),
            ApiError::HostAnswered(503),
            ApiError::HostAnswered(407),
            ApiError::TokenRefused,
            ApiError::ManagementOff,
            ApiError::ReadOnly(String::new()),
            ApiError::TooOld,
            ApiError::Unsupported,
            ApiError::NotFound("x".into()),
            ApiError::Conflict("x".into()),
            ApiError::Invalid("x".into()),
            ApiError::Server(500, String::new()),
            ApiError::NotKept,
            ApiError::Keystore("locked".into()),
        ];
        for l in Lang::ALL {
            for e in &all {
                let text = e.explain(l);
                assert!(!text.is_empty() && !text.contains("{}"), "{l:?} {e:?}: {text}");
            }
            for e in [
                TokenError::Empty,
                TokenError::Short,
                TokenError::Password,
                TokenError::Application,
                TokenError::Characters,
            ] {
                assert!(!e.explain(l).is_empty());
            }
        }
        assert_eq!(seconds(Duration::from_millis(300)), "300 ms");
        assert_eq!(seconds(Duration::from_secs(45)), "45 s");
    }

    #[test]
    fn a_typed_token() {
        let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        for typed in [
            token.to_owned(),
            format!("  {token}\n"),
            format!("NUAGE_ADMIN_TOKEN={token}"),
            format!("NUAGE_ADMIN_TOKEN=\"{token}\""),
            format!("export NUAGE_ADMIN_TOKEN = '{token}'"),
        ] {
            assert_eq!(typed_token(&typed).map(|s| s.expose().to_owned()), Ok(token.to_owned()), "{typed}");
        }
        assert_eq!(typed_token("  ").map(|_| ()), Err(TokenError::Empty));
        assert_eq!(typed_token("short").map(|_| ()), Err(TokenError::Short));
        // The installer's console password (24 characters): the hint names it.
        assert_eq!(typed_token("0123456789abcdef01234567").map(|_| ()), Err(TokenError::Password));
        assert!(TokenError::Password.explain(Lang::Fr).contains("NUAGE_ADMIN_PASSWORD"));
        assert_eq!(typed_token("nuage_0123456789abcdef0123456789abcdef").map(|_| ()), Err(TokenError::Application));
        assert_eq!(typed_token("0123456789abcdef 0123456789abcdef").map(|_| ()), Err(TokenError::Characters));
        assert_eq!(segment("ma boutique/é"), "ma%20boutique%2F%C3%A9");
    }

    #[test]
    fn names_and_websites() {
        assert_eq!(suggest_name("Ma Boutique Été"), "ma-boutique-ete");
        assert_eq!(suggest_name("  L'atelier — Ñandú & Cie  "), "l-atelier-nandu-cie");
        assert_eq!(suggest_name("blog_2"), "blog_2");
        assert_eq!(suggest_name("!!!"), "");
        let long = suggest_name(&"a".repeat(70));
        assert_eq!(long.len(), 63);
        assert!(app_name_ok(&long) && app_name_ok("ma-boutique") && app_name_ok("a_1"));
        for bad in ["", "main", "Shop", "ma boutique", "é", &"a".repeat(64)] {
            assert!(!app_name_ok(bad), "{bad}");
        }
        assert_eq!(origin(" HTTPS://MonSite.fr/ ").as_deref(), Some("https://monsite.fr"));
        assert_eq!(origin("http://localhost:3000").as_deref(), Some("http://localhost:3000"));
        assert_eq!(origin("*").as_deref(), Some("*"));
        for bad in ["monsite.fr", "https://", "https://monsite.fr/page", "ftp://x", "https://a b", "https://a,b"] {
            assert_eq!(origin(bad), None, "{bad}");
        }
        let status: Status = serde_json::from_value(json!({
            "version": "0.4.7", "uptime_s": 90, "memory": { "process_bytes": 5, "total_bytes": 9 },
            "backup": { "enabled": true, "target": "/var/backups", "last_at": "2026-11-15T21:04:00Z", "last_ok": true, "last_error": null },
            "databases": [{ "name": "shop", "status": "ok", "tables": 2, "rows": 3, "websites": ["https://x.fr"],
                            "last_connection": { "at": "2026-11-15T21:04:00Z", "via": "HTTP API", "client": "nuage-editor/0.4.7" } }],
        }))
        .unwrap();
        assert!(
            status.backup.last_ok && status.database("shop").unwrap().last_connection.as_ref().unwrap().by_the_editor()
        );
        let public: Status =
            serde_json::from_value(json!({ "name": "Nuage DB", "status": "ok", "uptime_s": 3 })).unwrap();
        assert!(public.version.is_empty(), "the public subset has no version");
    }

    #[test]
    fn the_token_is_never_printed() {
        let server =
            Server::new("localhost:8080", Secret::new("made-up-token".into()), Lang::Fr, &Transport::default())
                .unwrap();
        assert_eq!(server.console_url(), "http://localhost:8080/admin");
        assert!(!format!("{server:?}").contains("made-up-token"));
        assert!(matches!(
            Server::new("ftp://x", Secret::new("t".into()), Lang::En, &Transport::default()),
            Err(ApiError::BadAddress(_))
        ));
        let public = server.clone().with_public(Some("https://nuage.example.com/".into()));
        assert_eq!(
            (server.public_address(), public.public_address()),
            ("http://localhost:8080", "https://nuage.example.com")
        );
    }

    #[test]
    fn this_computer_and_its_network_are_called_without_a_proxy() {
        // Whatever HTTPS_PROXY says: never through it (tests/fake_servers.rs calls with one set).
        for address in ["http://127.0.0.1:8080", "localhost:8080", "http://192.168.1.20:8080", "nas:8080"] {
            let server = Server::new(address, Secret::new("t".into()), Lang::En, &Transport::default()).unwrap();
            assert!(server.agent.config().proxy().is_none(), "{address}");
        }
    }

    #[test]
    fn a_name_no_computer_has() {
        let server =
            Server::new("https://nuage-typo.example.com", Secret::new("t".into()), Lang::En, &Transport::default())
                .unwrap();
        // What std gives ureq when the system's lookup fails, on each system.
        let failed = if cfg!(windows) {
            std::io::Error::from_raw_os_error(11001)
        } else {
            std::io::Error::other("failed to lookup address information: Name or service not known")
        };
        assert_eq!(
            server.failed(ureq::Error::Io(failed), true),
            ApiError::HostNotFound("nuage-typo.example.com".into())
        );
        assert_eq!(
            server.failed(ureq::Error::HostNotFound, true),
            ApiError::HostNotFound("nuage-typo.example.com".into())
        );
        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset by peer");
        assert!(matches!(server.failed(ureq::Error::Io(reset), false), ApiError::Network(_)));
    }

    #[test]
    fn what_a_failure_says_of_the_server_and_of_a_change() {
        let on_the_way = [
            ApiError::Timeout { address: "x".into(), after: Duration::from_secs(45) },
            ApiError::HostAnswered(503),
            ApiError::Network("connection reset".into()),
            ApiError::NotNuage("x".into()),
            ApiError::TooLarge,
            ApiError::Bug("x".into()),
            ApiError::Server(500, "disk full".into()),
        ];
        for e in &on_the_way {
            assert!(e.change_unknown() && !e.from_the_server(), "{e:?}");
        }
        let refusals = [
            ApiError::TokenRefused,
            ApiError::ManagementOff,
            ApiError::Unsupported,
            ApiError::NotFound("x".into()),
            ApiError::Conflict("x".into()),
            ApiError::Invalid("x".into()),
        ];
        for e in &refusals {
            assert!(!e.change_unknown() && e.from_the_server(), "{e:?}");
        }
        // Never sent: nothing was made, and nothing is known of the server either.
        for e in [
            ApiError::Busy,
            ApiError::Refused { address: "x".into(), loopback: true, port: 1 },
            ApiError::HostAnswered(407),
        ] {
            assert!(!e.change_unknown() && !e.from_the_server(), "{e:?}");
        }
    }
}
