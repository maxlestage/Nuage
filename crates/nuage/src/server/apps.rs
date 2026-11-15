//! Applications: several applications use one server, each with a database
//! of its own (its engine, its directory, its keys, its backups) and tokens
//! of its own. The token an application sends picks its database: one
//! application never sees another's data.
//!
//! The first application is the server's own engine (named `NUAGE_NAME`,
//! token `NUAGE_TOKEN`, data at the root of the data directory). The others
//! live in `apps/<name>/` and back up under `apps/<name>/` in the backup
//! location. Their list, with SHA-256 digests of their tokens (never the
//! tokens), is kept in the first application's metadata: written through its
//! log, encrypted and backed up with it, and restored before the others.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::{Metrics, random_token};
use crate::engine::{BackupOptions, Engine, PrefixedStore, backup, check_branch_name, store::now_millis};

const REGISTRY_PREFIX: &[u8] = b"app:";
/// A token's id: the first characters of the token, shown to tell tokens
/// apart (not enough to guess the rest).
const TOKEN_ID_LEN: usize = 8;
/// Tokens made by the server start with it (easy to spot in a leak scan).
const TOKEN_PREFIX: &str = "nuage_";

/// A token of an application, as stored: a digest, never the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenInfo {
    /// The token's first characters (`3f9a12c0`).
    pub id: String,
    /// When it was made (Unix ms).
    pub created_at: i64,
    /// Reads only: for the JavaScript of a website, which anyone can read.
    pub read_only: bool,
    digest: [u8; 32],
}

/// An application served by the server.
pub(crate) struct App {
    pub name: String,
    pub description: Mutex<String>,
    pub engine: Arc<Engine>,
    /// The server's own engine (its tokens may include `NUAGE_TOKEN`).
    pub first: bool,
    pub created_at: i64,
    tokens: Mutex<Vec<TokenInfo>>,
    /// Websites whose pages may call it from the browser (CORS): `https://site.fr`, or `*`.
    origins: RwLock<Vec<String>>,
    pub metrics: Metrics,
    /// Last request (Unix ms, 0: none yet).
    pub last_request: AtomicI64,
    /// Who connected last, and how: the console shows it.
    pub last_connection: Mutex<Option<Connection>>,
    /// Its key-value store, in memory (`/v1/kv`, the key-value port).
    pub kv: crate::kv::Kv,
}

/// A connection of an application's client, as the console shows it.
#[derive(Clone, Debug)]
pub(crate) struct Connection {
    /// When (Unix ms).
    pub at: i64,
    /// `HTTP API`, `SQL over HTTP` or `PostgreSQL`.
    pub via: &'static str,
    /// The client's name: its User-Agent, a website's origin, or a
    /// PostgreSQL driver's `application_name`.
    pub client: String,
    /// Its address.
    pub peer: String,
    pub read_only: bool,
}

/// What the console and the status show of an application.
#[derive(Clone, Debug, PartialEq)]
pub struct AppInfo {
    /// Its name (also the name of its database).
    pub name: String,
    /// A note from whoever made it.
    pub description: String,
    /// The first application: the server's own engine.
    pub first: bool,
    /// When it was made (Unix ms; the server's start for the first).
    pub created_at: i64,
    /// Its tokens.
    pub tokens: Vec<TokenInfo>,
    /// `NUAGE_TOKEN` opens it too (the first application only).
    pub env_token: bool,
    /// Websites whose pages may call it from the browser.
    pub origins: Vec<String>,
}

fn digest(token: &str) -> [u8; 32] {
    crate::hash::sha256(token.as_bytes())
}

/// Two digests are the same. Every byte is compared: the time taken does not
/// say where they differ. (Digests, not secrets: no need to hash them again.)
fn same_digest(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let v: Option<Vec<u8>> = (0..64).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect();
    v?.try_into().ok()
}

impl App {
    fn new(
        name: String,
        engine: Arc<Engine>,
        first: bool,
        created_at: i64,
        description: String,
        kv_bytes: usize,
    ) -> App {
        App {
            kv: crate::kv::Kv::new(kv_bytes),
            name,
            description: Mutex::new(description),
            engine,
            first,
            created_at,
            tokens: Mutex::new(Vec::new()),
            origins: RwLock::new(Vec::new()),
            metrics: Metrics::new(),
            last_request: AtomicI64::new(0),
            last_connection: Mutex::new(None),
        }
    }

    pub(crate) fn tokens(&self) -> Vec<TokenInfo> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `Some(read_only)` when the token with digest `d` is one of this
    /// application's (the environment's token aside).
    fn token(&self, d: &[u8; 32]) -> Option<bool> {
        // Every digest is compared: the time taken tells nothing.
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .fold(None, |found, t| if same_digest(&t.digest, d) { Some(t.read_only) } else { found })
    }

    /// The websites allowed to call it from the browser.
    pub(crate) fn origins(&self) -> Vec<String> {
        self.origins.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A page of `origin` may call it.
    pub(crate) fn allows_origin(&self, origin: &str) -> bool {
        let origin = normalize_origin(origin);
        self.origins.read().unwrap_or_else(|e| e.into_inner()).iter().any(|o| o == "*" || *o == origin)
    }

    pub(crate) fn touch(&self) {
        self.last_request.store(now_millis(), Ordering::Relaxed);
    }

    /// A client of this application connected (a request, or a PostgreSQL login).
    pub(crate) fn connected(&self, via: &'static str, client: &str, peer: &str, read_only: bool) {
        let client: String = client.chars().take(80).collect();
        *self.last_connection.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Connection { at: now_millis(), via, client, peer: peer.chars().take(64).collect(), read_only });
    }

    pub(crate) fn last_connection(&self) -> Option<Connection> {
        self.last_connection.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn record(&self) -> Json {
        let tokens: Vec<Json> = self
            .tokens()
            .iter()
            .map(
                |t| json!({ "id": t.id, "createdAt": t.created_at, "sha256": hex(&t.digest), "readOnly": t.read_only }),
            )
            .collect();
        json!({
            "createdAt": self.created_at,
            "description": *self.description.lock().unwrap_or_else(|e| e.into_inner()),
            "tokens": tokens,
            "origins": self.origins(),
        })
    }
}

/// Every application of a server.
pub(crate) struct Apps {
    first: Arc<App>,
    others: RwLock<BTreeMap<String, Arc<App>>>,
    /// Applications that could not be opened at startup, and why.
    failed: Mutex<Vec<(String, String)>>,
    /// Serializes creations and deletions.
    changes: Mutex<()>,
    /// Memory of each application's key-value store.
    kv_bytes: usize,
    /// Memory the key-value stores of all the applications may hold
    /// together (`None`: each has `kv_bytes`, whatever their number).
    kv_budget: Option<usize>,
    /// Memory the page caches of all the applications may hold together
    /// (`None`: each engine keeps the size it was opened with).
    cache_budget: Option<usize>,
    /// The most the budget may grow to while memory has room.
    cache_ceiling: usize,
    /// The budget in force: it starts at `cache_budget`, then follows the
    /// memory (see [`crate::memory::cache_move`]).
    cache_in_force: std::sync::atomic::AtomicUsize,
}

impl Apps {
    /// The applications of `engine` (the first one): opens every other
    /// application it lists. One that cannot be opened is reported, not
    /// fatal.
    pub(crate) fn open(
        engine: Arc<Engine>,
        kv_bytes: usize,
        kv_budget: Option<usize>,
        cache_budget: Option<usize>,
        cache_ceiling: Option<usize>,
    ) -> Apps {
        let first = Arc::new(App::new(
            engine.options().name.clone(),
            engine.clone(),
            true,
            crate::engine::store::now_millis() - engine.uptime().as_millis() as i64,
            String::new(),
            kv_bytes,
        ));
        let apps = Apps {
            first,
            others: RwLock::new(BTreeMap::new()),
            failed: Mutex::new(Vec::new()),
            changes: Mutex::new(()),
            kv_bytes,
            kv_budget,
            cache_budget,
            cache_ceiling: cache_ceiling.unwrap_or(0).max(cache_budget.unwrap_or(0)),
            cache_in_force: std::sync::atomic::AtomicUsize::new(cache_budget.unwrap_or(0)),
        };
        let entries = match engine.meta_entries(REGISTRY_PREFIX) {
            Ok(e) => e,
            Err(e) => {
                engine.event("error", format!("cannot read the list of applications: {e}"));
                Vec::new()
            }
        };
        for (key, value) in entries {
            let name = String::from_utf8_lossy(&key[REGISTRY_PREFIX.len()..]).into_owned();
            let record: Json = serde_json::from_slice(&value).unwrap_or(Json::Null);
            let tokens = parse_tokens(&record);
            if name == apps.first.name {
                *apps.first.tokens.lock().unwrap_or_else(|e| e.into_inner()) = tokens;
                *apps.first.description.lock().unwrap_or_else(|e| e.into_inner()) = description_of(&record);
                *apps.first.origins.write().unwrap_or_else(|e| e.into_inner()) = origins_of(&record);
                continue;
            }
            match apps.open_engine(&name) {
                Ok(e) => {
                    let created = record.get("createdAt").and_then(Json::as_i64).unwrap_or(0);
                    let app = App::new(name.clone(), e, false, created, description_of(&record), kv_bytes);
                    *app.tokens.lock().unwrap_or_else(|e| e.into_inner()) = tokens;
                    *app.origins.write().unwrap_or_else(|e| e.into_inner()) = origins_of(&record);
                    apps.others.write().unwrap_or_else(|e| e.into_inner()).insert(name, Arc::new(app));
                }
                Err(e) => {
                    engine.event("error", format!("application `{name}` cannot be opened: {e}"));
                    apps.failed.lock().unwrap_or_else(|e| e.into_inner()).push((name, e.to_string()));
                }
            }
        }
        apps.share_cache();
        apps
    }

    /// Gives each application its part of the cache budget: what it holds,
    /// plus an equal part of what is free (see [`crate::memory::cache_shares`]),
    /// so the caches never outgrow the budget and it goes where it is used.
    /// Called when the applications change, and every few seconds.
    pub(crate) fn share_cache(&self) {
        let all = self.all();
        // The key-value stores too: together they keep to their budget.
        let kv = crate::memory::kv_share(self.kv_bytes, self.kv_budget, all.len());
        for app in &all {
            app.kv.set_max_bytes(kv);
        }
        if self.cache_budget.is_none() {
            return;
        }
        let budget = self.cache_in_force.load(Ordering::Relaxed);
        let used: Vec<usize> = all.iter().map(|app| app.engine.cache_used()).collect();
        for (app, share) in all.iter().zip(crate::memory::cache_shares(budget, &used)) {
            app.engine.set_cache_bytes(share);
        }
    }

    /// What the caches of all the applications hold now.
    pub(crate) fn cache_used(&self) -> usize {
        self.all().iter().map(|app| app.engine.cache_used()).sum()
    }

    /// The cache budget of the server, all applications together: the one
    /// in force (it follows the memory). `None`: there is no shared budget.
    pub(crate) fn cache_budget(&self) -> Option<usize> {
        self.cache_budget.map(|_| self.cache_in_force.load(Ordering::Relaxed))
    }

    /// The most the budget may grow to.
    pub(crate) fn cache_ceiling(&self) -> Option<usize> {
        self.cache_budget.map(|_| self.cache_ceiling)
    }

    /// Puts a new budget in force and shares it.
    pub(crate) fn set_cache_budget(&self, bytes: usize) {
        if self.cache_budget.is_some() {
            self.cache_in_force.store(bytes, Ordering::Relaxed);
            self.share_cache();
        }
    }

    /// The caches are full: a larger budget would be used. Full is three
    /// quarters of it: a cache lets go of pages by whole sixteenths of itself,
    /// so one that turns pages away sits between there and its limit.
    pub(crate) fn cache_demand(&self) -> bool {
        let budget = self.cache_in_force.load(Ordering::Relaxed);
        budget > 0 && self.cache_used() >= budget / 4 * 3
    }

    /// Lets go of every application's cached pages (memory runs short).
    pub(crate) fn drop_caches(&self) {
        for app in self.all() {
            app.engine.drop_cache();
        }
    }

    pub(crate) fn first(&self) -> &Arc<App> {
        &self.first
    }

    /// Every application, the first one first, then by name.
    pub(crate) fn all(&self) -> Vec<Arc<App>> {
        let mut out = vec![self.first.clone()];
        out.extend(self.others.read().unwrap_or_else(|e| e.into_inner()).values().cloned());
        out
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<App>> {
        if name == self.first.name {
            return Some(self.first.clone());
        }
        self.others.read().unwrap_or_else(|e| e.into_inner()).get(name).cloned()
    }

    pub(crate) fn failed(&self) -> Vec<(String, String)> {
        self.failed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The application `token` opens, and whether it only reads:
    /// `env_token` (`NUAGE_TOKEN`) opens the first one, for everything. Every
    /// application is tried, whatever matches.
    pub(crate) fn authenticate(&self, token: &str, env_token: Option<&str>) -> Option<(Arc<App>, bool)> {
        let mut found = None;
        // One hash of what was sent, compared to the hash of each secret.
        let d = digest(token);
        if env_token.is_some_and(|t| same_digest(&digest(t), &d)) {
            found = Some((self.first.clone(), false));
        }
        for app in self.all() {
            if let Some(read_only) = app.token(&d)
                && found.is_none()
            {
                found = Some((app, read_only));
            }
        }
        found
    }

    /// Some application lets pages of `origin` call it (the answer to a
    /// browser's preflight, which carries no token).
    pub(crate) fn any_allows(&self, origin: &str) -> bool {
        self.all().iter().any(|a| a.allows_origin(origin))
    }

    /// Sets the websites whose pages may call `app`, one per line or comma:
    /// `https://site.fr`, `http://localhost:3000`, or `*` for any.
    pub(crate) fn set_origins(&self, app: &App, list: &str) -> Result<Vec<String>> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        let mut origins = Vec::new();
        for o in list.split(|c: char| c == ',' || c.is_whitespace()).filter(|o| !o.is_empty()) {
            let o = normalize_origin(o);
            let valid = o == "*"
                || ((o.starts_with("https://") || o.starts_with("http://"))
                    && o.split("://").nth(1).is_some_and(|h| !h.is_empty() && !h.contains('/')));
            if !valid {
                return Err(Error::Validation(format!(
                    "`{o}` is not an origin: write `https://site.fr` (scheme and host, no path), or `*`"
                )));
            }
            if !origins.contains(&o) {
                origins.push(o);
            }
        }
        *app.origins.write().unwrap_or_else(|e| e.into_inner()) = origins.clone();
        self.save(app)?;
        self.first.engine.event("info", format!("websites of application `{}`: {}", app.name, origins.join(", ")));
        Ok(origins)
    }

    /// Any application has a token (else the database API is off).
    pub(crate) fn any_token(&self) -> bool {
        self.all().iter().any(|a| !a.tokens.lock().unwrap_or_else(|e| e.into_inner()).is_empty())
    }

    fn open_engine(&self, name: &str) -> Result<Arc<Engine>> {
        let first = &self.first.engine;
        let mut options = first.options().clone();
        options.name = name.to_owned();
        if let Some(budget) = self.cache_budget() {
            // Its part once it is there; `share_cache` then evens the others.
            options.cache_bytes = crate::memory::cache_share(budget, self.all().len() + 1);
        }
        options.backup = options.backup.map(|b| {
            let store: Arc<dyn crate::engine::BackupStore> =
                Arc::new(PrefixedStore::new(b.store.clone(), format!("apps/{name}/")));
            BackupOptions { store, ..b }
        });
        Engine::open(dir_of(first, name), options)
    }

    fn save(&self, app: &App) -> Result<()> {
        let mut key = REGISTRY_PREFIX.to_vec();
        key.extend_from_slice(app.name.as_bytes());
        self.first.engine.set_meta(&key, Some(app.record().to_string().as_bytes()))
    }

    /// Creates an application and its first token. Returns the token: it is
    /// shown once, only its digest is kept.
    pub(crate) fn create(&self, name: &str, description: &str) -> Result<(Arc<App>, String)> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        check_branch_name(name).map_err(|_| {
            Error::Validation(format!(
                "invalid application name `{name}`: use 1 to 63 lowercase letters, digits, `-` and `_`"
            ))
        })?;
        if self.get(name).is_some() || self.failed().iter().any(|(n, _)| n == name) {
            return Err(Error::Validation(format!("there is already an application named `{name}`")));
        }
        let dir = dir_of(&self.first.engine, name);
        if dir.exists() {
            return Err(Error::Validation(format!(
                "{} already exists: remove it, or pick another name",
                dir.display()
            )));
        }
        let engine = self.open_engine(name)?;
        let app = Arc::new(App::new(
            name.to_owned(),
            engine,
            false,
            now_millis(),
            description.trim().to_owned(),
            self.kv_bytes,
        ));
        let token = add_token(&app, false);
        self.save(&app)?;
        self.others.write().unwrap_or_else(|e| e.into_inner()).insert(name.to_owned(), app.clone());
        self.share_cache();
        self.first.engine.event("info", format!("application `{name}` created"));
        Ok((app, token))
    }

    /// A new token for `app` (`read_only`: it reads, never writes), shown
    /// once.
    pub(crate) fn new_token(&self, app: &App, read_only: bool) -> Result<String> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        let token = add_token(app, read_only);
        self.save(app)?;
        let kind = if read_only { "read-only token" } else { "token" };
        self.first.engine.event("info", format!("new {kind} for application `{}`", app.name));
        Ok(token)
    }

    /// Revokes the token `id` of `app`: `false` when it has none such.
    pub(crate) fn revoke_token(&self, app: &App, id: &str) -> Result<bool> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        let removed = {
            let mut tokens = app.tokens.lock().unwrap_or_else(|e| e.into_inner());
            let before = tokens.len();
            tokens.retain(|t| t.id != id);
            before != tokens.len()
        };
        if removed {
            self.save(app)?;
            self.first.engine.event("warn", format!("token {id}… of application `{}` revoked", app.name));
        }
        Ok(removed)
    }

    pub(crate) fn set_description(&self, app: &App, description: &str) -> Result<()> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        *app.description.lock().unwrap_or_else(|e| e.into_inner()) = description.trim().to_owned();
        self.save(app)
    }

    /// Deletes an application: its tokens, its data and its backups. The
    /// first application cannot be deleted.
    pub(crate) fn delete(&self, name: &str) -> Result<bool> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        if name == self.first.name {
            return Err(Error::Unsupported("the first application cannot be deleted".into()));
        }
        let app = self.others.write().unwrap_or_else(|e| e.into_inner()).remove(name);
        let was_failed = {
            let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
            let before = failed.len();
            failed.retain(|(n, _)| n != name);
            before != failed.len()
        };
        if app.is_none() && !was_failed {
            return Ok(false);
        }
        let mut key = REGISTRY_PREFIX.to_vec();
        key.extend_from_slice(name.as_bytes());
        self.first.engine.set_meta(&key, None)?;
        if let Some(app) = app {
            let _ = app.engine.close();
        }
        if let Some(b) = &self.first.engine.options().backup {
            let _ = backup::delete_all(&PrefixedStore::new(b.store.clone(), format!("apps/{name}/")));
        }
        let _ = std::fs::remove_dir_all(dir_of(&self.first.engine, name));
        self.share_cache();
        self.first.engine.event("warn", format!("application `{name}` deleted, with its data and backups"));
        Ok(true)
    }
}

fn dir_of(first: &Engine, name: &str) -> PathBuf {
    first.dir().join("apps").join(name)
}

/// `https://Site.fr/` → `https://site.fr`: what browsers send in `Origin`.
fn normalize_origin(o: &str) -> String {
    o.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn add_token(app: &App, read_only: bool) -> String {
    let token = format!("{TOKEN_PREFIX}{}", random_token());
    let info = TokenInfo {
        id: token[TOKEN_PREFIX.len()..TOKEN_PREFIX.len() + TOKEN_ID_LEN].to_owned(),
        created_at: now_millis(),
        read_only,
        digest: digest(&token),
    };
    app.tokens.lock().unwrap_or_else(|e| e.into_inner()).push(info);
    token
}

fn parse_tokens(record: &Json) -> Vec<TokenInfo> {
    record
        .get("tokens")
        .and_then(Json::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|t| {
                    Some(TokenInfo {
                        id: t.get("id")?.as_str()?.to_owned(),
                        created_at: t.get("createdAt").and_then(Json::as_i64).unwrap_or(0),
                        read_only: t.get("readOnly").and_then(Json::as_bool).unwrap_or(false),
                        digest: unhex(t.get("sha256")?.as_str()?)?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn origins_of(record: &Json) -> Vec<String> {
    record
        .get("origins")
        .and_then(Json::as_array)
        .map(|l| l.iter().filter_map(Json::as_str).map(normalize_origin).collect())
        .unwrap_or_default()
}

fn description_of(record: &Json) -> String {
    record.get("description").and_then(Json::as_str).unwrap_or_default().to_owned()
}

impl App {
    pub(crate) fn info(&self, env_token: bool) -> AppInfo {
        AppInfo {
            name: self.name.clone(),
            description: self.description.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            first: self.first,
            created_at: self.created_at,
            tokens: self.tokens(),
            env_token: self.first && env_token,
            origins: self.origins(),
        }
    }
}
