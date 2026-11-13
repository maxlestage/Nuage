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

use super::{Metrics, random_token, same_secret};
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
    pub metrics: Metrics,
    /// Last request (Unix ms, 0: none yet).
    pub last_request: AtomicI64,
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
}

fn digest(token: &str) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    d.as_ref().try_into().unwrap_or([0; 32])
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
    fn new(name: String, engine: Arc<Engine>, first: bool, created_at: i64, description: String) -> App {
        App {
            name,
            description: Mutex::new(description),
            engine,
            first,
            created_at,
            tokens: Mutex::new(Vec::new()),
            metrics: Metrics::new(),
            last_request: AtomicI64::new(0),
        }
    }

    pub(crate) fn tokens(&self) -> Vec<TokenInfo> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `token` is one of this application's (the environment's token aside).
    fn has_token(&self, token: &str) -> bool {
        let d = digest(token);
        // Every digest is compared: the time taken tells nothing.
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .fold(false, |found, t| found | same_secret(&t.digest, &d))
    }

    pub(crate) fn touch(&self) {
        self.last_request.store(now_millis(), Ordering::Relaxed);
    }

    fn record(&self) -> Json {
        let tokens: Vec<Json> = self
            .tokens()
            .iter()
            .map(|t| json!({ "id": t.id, "createdAt": t.created_at, "sha256": hex(&t.digest) }))
            .collect();
        json!({
            "createdAt": self.created_at,
            "description": *self.description.lock().unwrap_or_else(|e| e.into_inner()),
            "tokens": tokens,
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
}

impl Apps {
    /// The applications of `engine` (the first one): opens every other
    /// application it lists. One that cannot be opened is reported, not
    /// fatal.
    pub(crate) fn open(engine: Arc<Engine>) -> Apps {
        let first = Arc::new(App::new(
            engine.options().name.clone(),
            engine.clone(),
            true,
            crate::engine::store::now_millis() - engine.uptime().as_millis() as i64,
            String::new(),
        ));
        let apps = Apps {
            first,
            others: RwLock::new(BTreeMap::new()),
            failed: Mutex::new(Vec::new()),
            changes: Mutex::new(()),
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
                continue;
            }
            match apps.open_engine(&name) {
                Ok(e) => {
                    let created = record.get("createdAt").and_then(Json::as_i64).unwrap_or(0);
                    let app = App::new(name.clone(), e, false, created, description_of(&record));
                    *app.tokens.lock().unwrap_or_else(|e| e.into_inner()) = tokens;
                    apps.others.write().unwrap_or_else(|e| e.into_inner()).insert(name, Arc::new(app));
                }
                Err(e) => {
                    engine.event("error", format!("application `{name}` cannot be opened: {e}"));
                    apps.failed.lock().unwrap_or_else(|e| e.into_inner()).push((name, e.to_string()));
                }
            }
        }
        apps
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

    /// The application `token` opens: `env_token` (`NUAGE_TOKEN`) opens the
    /// first one. Every application is tried, whatever matches.
    pub(crate) fn authenticate(&self, token: &str, env_token: Option<&str>) -> Option<Arc<App>> {
        let mut found = None;
        if env_token.is_some_and(|t| same_secret(t.as_bytes(), token.as_bytes())) {
            found = Some(self.first.clone());
        }
        for app in self.all() {
            if app.has_token(token) && found.is_none() {
                found = Some(app);
            }
        }
        found
    }

    /// Any application has a token (else the database API is off).
    pub(crate) fn any_token(&self) -> bool {
        self.all().iter().any(|a| !a.tokens.lock().unwrap_or_else(|e| e.into_inner()).is_empty())
    }

    fn open_engine(&self, name: &str) -> Result<Arc<Engine>> {
        let first = &self.first.engine;
        let mut options = first.options().clone();
        options.name = name.to_owned();
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
        let app = Arc::new(App::new(name.to_owned(), engine, false, now_millis(), description.trim().to_owned()));
        let token = add_token(&app);
        self.save(&app)?;
        self.others.write().unwrap_or_else(|e| e.into_inner()).insert(name.to_owned(), app.clone());
        self.first.engine.event("info", format!("application `{name}` created"));
        Ok((app, token))
    }

    /// A new token for `app`, shown once.
    pub(crate) fn new_token(&self, app: &App) -> Result<String> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        let token = add_token(app);
        self.save(app)?;
        self.first.engine.event("info", format!("new token for application `{}`", app.name));
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
        self.first.engine.event("warn", format!("application `{name}` deleted, with its data and backups"));
        Ok(true)
    }
}

fn dir_of(first: &Engine, name: &str) -> PathBuf {
    first.dir().join("apps").join(name)
}

fn add_token(app: &App) -> String {
    let token = format!("{TOKEN_PREFIX}{}", random_token());
    let info = TokenInfo {
        id: token[TOKEN_PREFIX.len()..TOKEN_PREFIX.len() + TOKEN_ID_LEN].to_owned(),
        created_at: now_millis(),
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
                        digest: unhex(t.get("sha256")?.as_str()?)?,
                    })
                })
                .collect()
        })
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
        }
    }
}
