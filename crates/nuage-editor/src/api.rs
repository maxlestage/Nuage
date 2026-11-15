//! What the editor asks a Nuage DB server, over its management API
//! (`/api/apps`, `/api/settings`), with its management token
//! (`NUAGE_ADMIN_TOKEN`). Every call is blocking: the window runs them on a
//! thread of their own (see [`crate::jobs`]), never on its own.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::i18n::Lang;

/// How long a server may take to answer a call. Saving a setting makes the
/// server send it to its backup at once, which may take a few seconds.
const TIMEOUT: Duration = Duration::from_secs(30);
/// How long reaching a server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// Why a call failed, as the window says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// No server answered at this address (nothing listens there, it is down,
    /// or the network is cut).
    Unreachable(String),
    /// The address answered, but not as a Nuage DB server.
    NotNuage(String),
    /// The server refused the management token.
    Unauthorized(String),
    /// The management API is off on this server, or what was asked is not
    /// allowed (the settings come from its environment only).
    Forbidden(String),
    /// A value that does not fit: the server's message says why, with the figures.
    Refused(String),
    /// No such application (deleted from the console meanwhile?).
    NotFound(String),
    /// Anything else (the server failed): its message.
    Failed(String),
}

impl ApiError {
    /// What the server or the network said.
    pub fn detail(&self) -> &str {
        match self {
            ApiError::Unreachable(m)
            | ApiError::NotNuage(m)
            | ApiError::Unauthorized(m)
            | ApiError::Forbidden(m)
            | ApiError::Refused(m)
            | ApiError::NotFound(m)
            | ApiError::Failed(m) => m,
        }
    }
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

/// A setting, as `/api/settings` gives it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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
    #[serde(default)]
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
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Startup {
    pub variable: String,
    /// `***` for a secret that is set; `None` when nothing was said.
    pub value: Option<String>,
}

/// Everything `/api/settings` answers.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Settings {
    /// The server takes its settings from its environment only.
    pub read_only: bool,
    /// What was saved and is not in force (`read_only`).
    #[serde(default)]
    pub ignored: BTreeMap<String, String>,
    pub settings: Vec<Setting>,
    pub memory: Memory,
    pub applications: Vec<AppMemory>,
    #[serde(default)]
    pub startup: Vec<Startup>,
    /// The settings file the server was started with.
    pub settings_file: Option<String>,
    /// Saved and in force, but not yet in the backup.
    #[serde(default)]
    pub warning: Option<String>,
}

impl Settings {
    /// The setting named `key`.
    pub fn get(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key == key)
    }
}

/// A token of an application: never the token itself.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Token {
    /// Its first characters, shown to tell tokens apart.
    pub id: String,
    pub created_at: i64,
    pub read_only: bool,
}

/// The memory of an application, as `/api/apps` gives it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct AppAmounts {
    #[serde(deserialize_with = "amount", default)]
    pub cache_mb: Amount,
    #[serde(deserialize_with = "amount", default)]
    pub kv_mb: Amount,
}

/// An application of the server.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Application {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The server's own application (`NUAGE_NAME`).
    #[serde(default)]
    pub first: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub tokens: Vec<Token>,
    /// `NUAGE_TOKEN` opens it too.
    #[serde(default)]
    pub env_token: bool,
    /// Websites whose pages may call it.
    #[serde(default)]
    pub origins: Vec<String>,
    #[serde(default)]
    pub memory: AppAmounts,
}

/// What an application needs to connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The token, shown once: the server keeps only its digest.
    pub token: String,
    /// `NUAGE_URL`.
    pub url: String,
}

impl Connection {
    /// The two lines an application's settings take.
    pub fn env_lines(&self) -> String {
        format!("NUAGE_URL={}\nNUAGE_TOKEN={}", self.url, self.token)
    }
}

/// A Nuage DB server and its management token.
#[derive(Clone)]
pub struct Server {
    base: String,
    token: String,
    lang: Lang,
    agent: ureq::Agent,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the token.
        f.debug_struct("Server").field("base", &self.base).finish_non_exhaustive()
    }
}

/// An address as a person types it: `localhost:8080`, `nuage.example.com`,
/// `https://…/admin` → `http://localhost:8080`, `https://nuage.example.com`.
pub fn normalize_address(typed: &str) -> String {
    let typed = typed.trim().trim_end_matches('/');
    let with_scheme = if typed.starts_with("http://") || typed.starts_with("https://") {
        typed.to_owned()
    } else {
        // A server of this computer or of the local network answers in clear;
        // anything else is reached over HTTPS.
        let host = typed.split(['/', ':']).next().unwrap_or_default();
        let local = host == "localhost"
            || host.starts_with("127.")
            || host.starts_with("192.168.")
            || host.starts_with("10.")
            || host.ends_with(".local");
        format!("{}://{typed}", if local { "http" } else { "https" })
    };
    // What follows the host (a page of the console or of the site) is not the server's address.
    match with_scheme.find("://").and_then(|i| with_scheme[i + 3..].find('/').map(|j| i + 3 + j)) {
        Some(path) => with_scheme[..path].to_owned(),
        None => with_scheme,
    }
}

impl Server {
    /// The server at `address` (normalized), opened with `token`. Its
    /// refusals come back in `lang`.
    pub fn new(address: &str, token: &str, lang: Lang) -> Server {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .http_status_as_error(false)
            .user_agent(concat!("nuage-editor/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Server { base: normalize_address(address), token: token.trim().to_owned(), lang, agent }
    }

    /// Its address.
    pub fn address(&self) -> &str {
        &self.base
    }

    /// Its console, in a browser.
    pub fn console_url(&self) -> String {
        format!("{}/admin", self.base)
    }

    /// One call: the answer's JSON, or why it failed.
    fn call(&self, method: &str, path: &str, body: Option<Json>) -> Result<Json, ApiError> {
        let url = format!("{}{path}", self.base);
        let auth = format!("Bearer {}", self.token);
        let lang = self.lang.code();
        let answer = match (method, body) {
            ("GET", _) => self.agent.get(&url).header("authorization", &auth).header("accept-language", lang).call(),
            ("DELETE", _) => {
                self.agent.delete(&url).header("authorization", &auth).header("accept-language", lang).call()
            }
            (_, body) => self
                .agent
                .post(&url)
                .header("authorization", &auth)
                .header("accept-language", lang)
                .send_json(body.unwrap_or_else(|| json!({}))),
        };
        let mut answer = answer.map_err(|e| ApiError::Unreachable(e.to_string()))?;
        let status = answer.status().as_u16();
        let is_json = answer
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/json"));
        let text = answer.body_mut().with_config().limit(32 << 20).read_to_string().unwrap_or_default();
        let json: Json = if is_json { serde_json::from_str(&text).unwrap_or(Json::Null) } else { Json::Null };
        let message = json["error"]["message"].as_str().unwrap_or_default().to_owned();
        match status {
            200..=299 if is_json => Ok(json),
            // A page instead of JSON: a website, a proxy's error page, a console page.
            200..=299 => {
                Err(ApiError::NotNuage(format!("{} answered with a web page, not with Nuage DB's API", self.base)))
            }
            401 => Err(ApiError::Unauthorized(message)),
            403 => Err(ApiError::Forbidden(message)),
            404 if message.is_empty() || message.starts_with("no such route") => Err(ApiError::NotNuage(format!(
                "{} has no management API: not a Nuage DB server, or a version before 0.4.2 (the settings came with 0.4.7)",
                self.base
            ))),
            404 => Err(ApiError::NotFound(message)),
            400 | 409 | 413 => Err(ApiError::Refused(message)),
            _ if !is_json => Err(ApiError::NotNuage(format!("{} answered {status}", self.base))),
            _ => Err(ApiError::Failed(if message.is_empty() { format!("HTTP {status}") } else { message })),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(json: Json) -> Result<T, ApiError> {
        serde_json::from_value(json)
            .map_err(|e| ApiError::NotNuage(format!("an answer this editor does not understand: {e}")))
    }

    /// The settings, the memory and the applications' amounts. Also what
    /// "check the connection" calls: it needs the management token.
    pub fn settings(&self) -> Result<Settings, ApiError> {
        Self::parse(self.call("GET", "/api/settings", None)?)
    }

    /// Saves settings (`None`: back to the environment's value), all or none.
    pub fn save_settings(&self, changes: &[(&str, Option<String>)]) -> Result<Settings, ApiError> {
        let body: serde_json::Map<String, Json> =
            changes.iter().map(|(k, v)| ((*k).to_owned(), v.clone().map_or(Json::Null, Json::String))).collect();
        Self::parse(self.call("POST", "/api/settings", Some(Json::Object(body)))?)
    }

    /// Gives a setting back to the environment's value, or the default.
    pub fn reset_setting(&self, key: &str) -> Result<Settings, ApiError> {
        Self::parse(self.call("DELETE", &format!("/api/settings/{key}"), None)?)
    }

    /// Every application.
    pub fn applications(&self) -> Result<Vec<Application>, ApiError> {
        let json = self.call("GET", "/api/apps", None)?;
        Self::parse(json["applications"].clone())
    }

    /// A new application, with its first token.
    pub fn create_application(&self, name: &str, description: &str) -> Result<(Application, Connection), ApiError> {
        let json = self.call("POST", "/api/apps", Some(json!({ "name": name.trim(), "description": description })))?;
        let app = Self::parse(json["application"].clone())?;
        Ok((app, self.connection_of(&json)))
    }

    /// Another token for `name`; `read_only`: it reads, never writes.
    pub fn new_token(&self, name: &str, read_only: bool) -> Result<Connection, ApiError> {
        let json = self.call("POST", &format!("/api/apps/{name}/tokens"), Some(json!({ "read_only": read_only })))?;
        Ok(self.connection_of(&json))
    }

    /// Revokes the token `id` of `name`: it opens nothing any more.
    pub fn revoke_token(&self, name: &str, id: &str) -> Result<(), ApiError> {
        self.call("DELETE", &format!("/api/apps/{name}/tokens/{id}"), None).map(|_| ())
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
        Self::parse(self.call("POST", &format!("/api/apps/{name}/memory"), Some(Json::Object(body)))?)
    }

    fn connection_of(&self, json: &Json) -> Connection {
        Connection {
            token: json["token"].as_str().unwrap_or_default().to_owned(),
            url: json["env"]["NUAGE_URL"].as_str().map_or_else(|| self.base.clone(), str::to_owned),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_as_people_type_them() {
        for (typed, address) in [
            ("localhost:8080", "http://localhost:8080"),
            ("  127.0.0.1:8080/ ", "http://127.0.0.1:8080"),
            ("192.168.1.20:8080", "http://192.168.1.20:8080"),
            ("nuage.example.com", "https://nuage.example.com"),
            ("https://nuage-f48a.herokuapp.com/admin/settings", "https://nuage-f48a.herokuapp.com"),
            ("http://maison.local:8080/fr/app/", "http://maison.local:8080"),
        ] {
            assert_eq!(normalize_address(typed), address, "{typed}");
        }
        assert_eq!(
            (mib_of("2 Go"), mib_of("1,5 Gio"), mib_of("512"), mib_of("auto")),
            (Some(2048), Some(1536), Some(512), None)
        );
        let server = Server::new("localhost:8080", " secret ", Lang::Fr);
        assert_eq!(server.console_url(), "http://localhost:8080/admin");
        assert!(!format!("{server:?}").contains("secret"), "the token is never printed");
    }

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
        let connection = Connection { token: "nuage_x".into(), url: "https://n.example".into() };
        assert_eq!(connection.env_lines(), "NUAGE_URL=https://n.example\nNUAGE_TOKEN=nuage_x");
    }

    #[test]
    fn nobody_there() {
        // A port nothing listens on (the first ones are reserved and closed).
        let err = Server::new("127.0.0.1:9", "t", Lang::En).settings().unwrap_err();
        assert!(matches!(err, ApiError::Unreachable(_)), "{err:?}");
    }
}
