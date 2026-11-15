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
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use memoire_orm::{Error, Result};
use serde_json::{Value as Json, json};

use super::settings::{Refusal, refused};
use super::{Metrics, random_token};
use crate::engine::{BackupOptions, Engine, PrefixedStore, backup, check_branch_name, store::now_millis};

const REGISTRY_PREFIX: &[u8] = b"app:";
/// A token's id: the first characters of the token, shown to tell tokens
/// apart (not enough to guess the rest).
const TOKEN_ID_LEN: usize = 8;
/// Tokens made by the server start with it (easy to spot in a leak scan).
const TOKEN_PREFIX: &str = "nuage_";

/// The least the page caches that share the budget keep together, whatever
/// is set apart for the databases that have an amount of their own.
const SHARED_FLOOR: usize = 16 << 20;
/// The least a database's own cache may be set to.
const MIN_CACHE: usize = 4 << 20;
/// The least a key-value store may be set to.
const MIN_KV: usize = 1 << 20;

/// How the memory of a server goes to its applications: the budgets of the
/// page caches and of the key-value stores. It changes when the owner changes
/// a memory setting ([`Apps::set_plan`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryPlan {
    /// Memory of each application's key-value store.
    pub kv_bytes: usize,
    /// Memory the key-value stores that follow the server's rule may hold
    /// together (`None`: each has `kv_bytes`, whatever their number).
    pub kv_budget: Option<usize>,
    /// `kv_bytes` was said by hand (not the automatic rule).
    pub kv_by_hand: bool,
    /// Memory all the key-value stores may hold together, those with an
    /// amount of their own included (`None`: no such limit).
    pub kv_room: Option<usize>,
    /// Memory the page caches of all the applications may hold together
    /// (`None`: each engine keeps the size it was opened with).
    pub cache_budget: Option<usize>,
    /// The most the budget may grow to.
    pub cache_ceiling: usize,
}

/// What is set by hand on a server (amounts, fixed budgets), checked
/// against its memory before a change is saved: the guard can take the page
/// caches down, but nobody takes back what the key-value stores hold.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ByHand {
    /// The memory the server counts on (`None`: unknown, nothing is checked).
    pub total: Option<u64>,
    /// The budget of the page caches, when it was set by hand.
    pub cache: Option<usize>,
    /// What is set apart for the caches of single databases.
    pub apart: usize,
    /// What the databases that share keep at least.
    pub floor: usize,
    /// The key-value stores with an amount of their own, in all.
    pub kv_own: usize,
    /// The amount of each store that follows the server's rule, when it was
    /// set by hand (`None`: the automatic rule, an eighth of the memory for
    /// all of them).
    pub kv_each: Option<usize>,
    /// How many stores follow the server's rule.
    pub kv_following: usize,
}

impl ByHand {
    /// Everything set by hand: the page caches and the key-value stores.
    pub(crate) fn amount(&self) -> usize {
        self.cache.unwrap_or(self.apart + self.floor) + self.kv()
    }

    /// All the key-value memory.
    fn kv(&self) -> usize {
        let following = match (self.kv_each, self.kv_following) {
            (_, 0) => 0,
            (Some(each), n) => each * n,
            (None, _) => self.total.map_or(0, |t| (t / 8) as usize),
        };
        self.kv_own + following
    }

    /// Refuses what does not fit: the key-value stores within a quarter of
    /// the memory, and with the page caches set by hand within three
    /// quarters. `what` names the change, for the refusal.
    pub(crate) fn check(&self, what: &str) -> std::result::Result<(), Refusal> {
        let Some(total) = self.total else { return Ok(()) };
        let mib = |bytes: usize| bytes >> 20;
        let (quarter, most) = ((total / 4) as usize, (total / 4 * 3) as usize);
        let kv = self.kv();
        if kv > quarter {
            return Err(Refusal::new(refused::KV_ALL_TOO_LARGE, &[&what, &mib(kv), &(total >> 20), &mib(quarter)]));
        }
        let caches = self.cache.unwrap_or(self.apart + self.floor);
        if caches + kv > most {
            return Err(Refusal::new(
                refused::BY_HAND_TOO_MUCH,
                &[&what, &mib(caches + kv), &mib(caches), &mib(kv), &(total >> 20), &mib(most)],
            ));
        }
        Ok(())
    }
}

/// What is set apart for the page caches of single databases.
struct Apart {
    /// What their owners asked, in all.
    asked: usize,
    /// What they have now.
    taken: usize,
    /// How many applications share the rest.
    sharing: usize,
    /// They have less than asked because the budget was cut (memory ran short).
    squeezed: bool,
}

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
    /// Memory set apart for its page cache, in bytes (0: its part of what
    /// the applications share).
    cache_reserved: AtomicUsize,
    /// Memory of its key-value store, in bytes (0: the server's rule).
    kv_fixed: AtomicUsize,
    /// The two amounts of its record when the server ignores what was saved
    /// (the environment is the only source of the settings): not in force,
    /// and kept in the record.
    amounts_kept: (AtomicUsize, AtomicUsize),
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
    /// Memory set apart for its page cache, in MiB (`None`: it shares the
    /// server's).
    pub cache_mb: Option<u64>,
    /// Memory of its key-value store, in MiB (`None`: the server's rule).
    pub kv_mb: Option<u64>,
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
            cache_reserved: AtomicUsize::new(0),
            kv_fixed: AtomicUsize::new(0),
            amounts_kept: (AtomicUsize::new(0), AtomicUsize::new(0)),
        }
    }

    /// Memory set apart for its page cache (0: it shares the server's).
    pub(crate) fn cache_reserved(&self) -> usize {
        self.cache_reserved.load(Ordering::Relaxed)
    }

    /// Memory of its key-value store when its owner set one (0: the server's rule).
    pub(crate) fn kv_fixed(&self) -> usize {
        self.kv_fixed.load(Ordering::Relaxed)
    }

    /// Takes the amounts of memory of its record; `ignored`: they are kept
    /// for the record, and not put in force.
    fn take_memory(&self, record: &Json, ignored: bool) {
        let mib = |key: &str| record.get(key).and_then(Json::as_u64).map_or(0, |m| (m as usize) << 20);
        let (cache, kv) =
            if ignored { (&self.amounts_kept.0, &self.amounts_kept.1) } else { (&self.cache_reserved, &self.kv_fixed) };
        cache.store(mib("cacheMb"), Ordering::Relaxed);
        kv.store(mib("kvMb"), Ordering::Relaxed);
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
        let kept = |amount: &AtomicUsize| amount.load(Ordering::Relaxed);
        self.record_with(
            self.cache_reserved().max(kept(&self.amounts_kept.0)),
            self.kv_fixed().max(kept(&self.amounts_kept.1)),
        )
    }

    /// Its record, with these amounts of memory (0: the server's rules).
    fn record_with(&self, cache: usize, kv: usize) -> Json {
        let tokens: Vec<Json> = self
            .tokens()
            .iter()
            .map(
                |t| json!({ "id": t.id, "createdAt": t.created_at, "sha256": hex(&t.digest), "readOnly": t.read_only }),
            )
            .collect();
        let mut record = json!({
            "createdAt": self.created_at,
            "description": *self.description.lock().unwrap_or_else(|e| e.into_inner()),
            "tokens": tokens,
            "origins": self.origins(),
        });
        // Amounts of memory its owner set: absent, it follows the server's rules.
        for (key, bytes) in [("cacheMb", cache), ("kvMb", kv)] {
            if bytes > 0 {
                record[key] = json!(bytes >> 20);
            }
        }
        record
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
    /// The budgets of the key-value stores and of the page caches.
    plan: RwLock<MemoryPlan>,
    /// The cache budget in force, what is set apart for single databases
    /// included: it starts at the plan's, then follows the memory (see
    /// [`crate::memory::cache_move`]).
    cache_in_force: AtomicUsize,
    /// What the memory guard learned: a budget that did not fit lately, the
    /// one in force stays under (0: nothing).
    cache_lesson: AtomicUsize,
}

impl Apps {
    /// The applications of `engine` (the first one): opens every other
    /// application it lists. One that cannot be opened is reported, not
    /// fatal.
    pub(crate) fn open(engine: Arc<Engine>, plan: MemoryPlan, ignore_amounts: bool) -> Apps {
        let kv_bytes = plan.kv_bytes;
        let plan = MemoryPlan { cache_ceiling: plan.cache_ceiling.max(plan.cache_budget.unwrap_or(0)), ..plan };
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
            plan: RwLock::new(plan),
            cache_in_force: AtomicUsize::new(plan.cache_budget.unwrap_or(0)),
            cache_lesson: AtomicUsize::new(0),
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
                apps.first.take_memory(&record, ignore_amounts);
                continue;
            }
            match apps.open_engine(&name) {
                Ok(e) => {
                    let created = record.get("createdAt").and_then(Json::as_i64).unwrap_or(0);
                    let app = App::new(name.clone(), e, false, created, description_of(&record), kv_bytes);
                    *app.tokens.lock().unwrap_or_else(|e| e.into_inner()) = tokens;
                    *app.origins.write().unwrap_or_else(|e| e.into_inner()) = origins_of(&record);
                    app.take_memory(&record, ignore_amounts);
                    apps.others.write().unwrap_or_else(|e| e.into_inner()).insert(name, Arc::new(app));
                }
                Err(e) => {
                    engine.event("error", format!("application `{name}` cannot be opened: {e}"));
                    apps.failed.lock().unwrap_or_else(|e| e.into_inner()).push((name, e.to_string()));
                }
            }
        }
        // What is set apart for single databases is part of the budget.
        apps.make_room();
        apps
    }

    /// The budgets in force.
    pub(crate) fn plan(&self) -> MemoryPlan {
        *self.plan.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Puts new budgets in force (the owner changed a memory setting), and
    /// shares everything anew. A budget that follows the memory goes on from
    /// where it was, within its new range; one set by hand starts at what was
    /// said. Returns whether anything changed.
    pub(crate) fn set_plan(&self, plan: MemoryPlan) -> bool {
        let plan = MemoryPlan { cache_ceiling: plan.cache_ceiling.max(plan.cache_budget.unwrap_or(0)), ..plan };
        let before = std::mem::replace(&mut *self.plan.write().unwrap_or_else(|e| e.into_inner()), plan);
        if before == plan {
            return false;
        }
        if (before.cache_budget, before.cache_ceiling) != (plan.cache_budget, plan.cache_ceiling) {
            let budget = plan.cache_budget.unwrap_or(0);
            let in_force = self.cache_in_force.load(Ordering::Relaxed);
            let by_hand = budget == plan.cache_ceiling;
            self.cache_in_force
                .store(if by_hand || before.cache_budget.is_none() { budget } else { in_force }, Ordering::Relaxed);
        }
        self.make_room();
        true
    }

    /// What is set apart for the page caches of single databases.
    fn set_apart(&self, all: &[Arc<App>], plan: &MemoryPlan) -> Apart {
        let asked: usize = all.iter().map(|a| a.cache_reserved()).sum();
        let sharing = all.iter().filter(|a| a.cache_reserved() == 0).count();
        if plan.cache_budget.is_none() {
            return Apart { asked, taken: asked, sharing, squeezed: false };
        }
        // Within the ceiling, the others keeping their floor (the memory may
        // have been lowered since the amounts were set)...
        let allowed = asked.min(plan.cache_ceiling.saturating_sub(if sharing > 0 { SHARED_FLOOR } else { 0 }));
        // ...and within the budget in force: cut because memory ran short,
        // it takes what is set apart down with it (2 MiB stay to each of the
        // others), and gives it back as it grows again.
        let taken = allowed.min(self.cache_in_force.load(Ordering::Relaxed).saturating_sub(sharing * (2 << 20)));
        Apart { asked, taken, sharing, squeezed: taken < allowed }
    }

    /// Gives each application its memory. The key-value stores: the amount
    /// its owner set, else the server's rule among those that follow it. The
    /// page caches: the amount set apart for a database, and for the others
    /// what each holds plus an equal part of what is free in the rest of the
    /// budget (see [`crate::memory::cache_shares`]), so the caches never
    /// outgrow the budget and it goes where it is used. Called when the
    /// applications or the settings change, and every few seconds.
    pub(crate) fn share_cache(&self) {
        let all = self.all();
        let plan = self.plan();
        // The stores with an amount of their own have it (in proportion when
        // all do not fit any more: the memory was lowered since); those that
        // follow the server's rule share what that leaves of the room.
        let following = all.iter().filter(|a| a.kv_fixed() == 0).count();
        let own_asked: usize = all.iter().map(|a| a.kv_fixed()).sum();
        let own = plan.kv_room.map_or(own_asked, |room| own_asked.min(room));
        let budget = match (plan.kv_budget, plan.kv_room) {
            (Some(budget), Some(room)) => Some(budget.min(room.saturating_sub(own))),
            (None, Some(room)) => Some(room.saturating_sub(own)),
            (budget, None) => budget,
        };
        let kv = crate::memory::kv_share(plan.kv_bytes, budget, following);
        for app in &all {
            let fixed = app.kv_fixed();
            let part = (fixed as u128 * own as u128 / own_asked.max(1) as u128) as usize;
            app.kv.set_max_bytes(if fixed > 0 { part.max(MIN_KV) } else { kv });
        }
        let apart = self.set_apart(&all, &plan);
        for app in all.iter().filter(|a| a.cache_reserved() > 0) {
            // Its amount; a part of it in proportion when all do not fit.
            let own = (app.cache_reserved() as u128 * apart.taken as u128 / apart.asked.max(1) as u128) as usize;
            app.engine.set_cache_bytes(own.max(2 << 20));
        }
        if plan.cache_budget.is_none() {
            return;
        }
        let shared = self.cache_in_force.load(Ordering::Relaxed).saturating_sub(apart.taken);
        let sharing: Vec<&Arc<App>> = all.iter().filter(|a| a.cache_reserved() == 0).collect();
        let used: Vec<usize> = sharing.iter().map(|app| app.engine.cache_used()).collect();
        for (app, share) in sharing.iter().zip(crate::memory::cache_shares(shared, &used)) {
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
        self.plan().cache_budget.map(|_| self.cache_in_force.load(Ordering::Relaxed))
    }

    /// The most the budget may grow to.
    pub(crate) fn cache_ceiling(&self) -> Option<usize> {
        let plan = self.plan();
        plan.cache_budget.map(|_| plan.cache_ceiling)
    }

    /// Puts a new budget in force (between 16 MiB and its ceiling, and under
    /// what the memory guard learned did not fit) and shares it.
    pub(crate) fn set_cache_budget(&self, bytes: usize) {
        let plan = self.plan();
        if plan.cache_budget.is_some() {
            let floor = SHARED_FLOOR.min(plan.cache_ceiling);
            let lesson = self.cache_lesson.load(Ordering::Relaxed);
            let top = if lesson == 0 { plan.cache_ceiling } else { plan.cache_ceiling.min(lesson) };
            self.cache_in_force.store(bytes.clamp(floor, top.max(floor)), Ordering::Relaxed);
        }
        self.share_cache();
    }

    /// What the memory guard learned: the budget stays under `bytes` until
    /// it says otherwise (0), whoever raises it (an amount set apart, a
    /// setting saved).
    pub(crate) fn set_cache_lesson(&self, bytes: usize) {
        self.cache_lesson.store(bytes, Ordering::Relaxed);
    }

    /// Makes the budget in force large enough for what is set apart (within
    /// its ceiling), and shares it: when the applications, their amounts or
    /// the plan change.
    fn make_room(&self) {
        let all = self.all();
        let asked: usize = all.iter().map(|a| a.cache_reserved()).sum();
        let floor = if all.iter().any(|a| a.cache_reserved() == 0) { SHARED_FLOOR } else { 0 };
        self.set_cache_budget(self.cache_in_force.load(Ordering::Relaxed).max(asked + floor));
    }

    /// A larger budget would be used: the caches that share it are full, or
    /// what is set apart for single databases was taken down with it and is
    /// to come back. Full is three quarters of their part: a cache lets go
    /// of pages by whole sixteenths of itself, so one that turns pages away
    /// sits between there and its limit.
    pub(crate) fn cache_demand(&self) -> bool {
        let all = self.all();
        let apart = self.set_apart(&all, &self.plan());
        let shared = self.cache_in_force.load(Ordering::Relaxed).saturating_sub(apart.taken);
        let used: usize = all.iter().filter(|a| a.cache_reserved() == 0).map(|a| a.engine.cache_used()).sum();
        apart.squeezed || (apart.sharing > 0 && shared > 0 && used >= shared / 4 * 3)
    }

    /// What is set apart for the page caches of single databases, as their
    /// owners asked, and what the others need to share at least.
    pub(crate) fn cache_set_apart(&self) -> (usize, usize) {
        let all = self.all();
        let asked = all.iter().map(|a| a.cache_reserved()).sum();
        (asked, if all.iter().any(|a| a.cache_reserved() == 0) { SHARED_FLOOR } else { 0 })
    }

    /// What is set by hand for `apps` under `plan`, to be checked against
    /// the memory before more is.
    pub(crate) fn by_hand(&self, apps: &[Arc<App>], plan: &MemoryPlan) -> ByHand {
        ByHand {
            total: crate::memory::MemoryInfo::now().total_bytes,
            // A budget that does not grow was said by hand.
            cache: plan.cache_budget.filter(|b| *b == plan.cache_ceiling),
            apart: apps.iter().map(|a| a.cache_reserved()).sum(),
            floor: if apps.iter().any(|a| a.cache_reserved() == 0) { SHARED_FLOOR } else { 0 },
            kv_own: apps.iter().map(|a| a.kv_fixed()).sum(),
            kv_each: plan.kv_by_hand.then_some(plan.kv_bytes),
            kv_following: apps.iter().filter(|a| a.kv_fixed() == 0).count(),
        }
    }

    /// What is set apart for single databases is cut for now (memory ran
    /// short): what they asked in all, and what they have.
    pub(crate) fn cache_squeezed(&self) -> Option<(usize, usize)> {
        let apart = self.set_apart(&self.all(), &self.plan());
        apart.squeezed.then_some((apart.asked, apart.taken))
    }

    /// Lets go of every application's cached pages (memory runs short).
    pub(crate) fn drop_caches(&self) {
        for app in self.all() {
            app.engine.drop_cache();
        }
    }

    /// Sets the memory of one application: `cache` bytes set apart for its
    /// page cache (`None`: it shares the server's budget with the others),
    /// `kv` bytes for its key-value store (`None`: the server's rule). What is
    /// set apart for all the databases must leave the others their floor
    /// within what the caches of this server may take.
    pub(crate) fn set_memory(
        &self,
        app: &App,
        cache: Option<usize>,
        kv: Option<usize>,
    ) -> std::result::Result<(), Refusal> {
        let _guard = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        let plan = self.plan();
        let others: Vec<Arc<App>> = self.all().into_iter().filter(|a| a.name != app.name).collect();
        let mib = |bytes: usize| bytes >> 20;
        if let Some(c) = cache {
            if c < MIN_CACHE {
                return Err(Refusal::new(refused::CACHE_TOO_SMALL, &[&mib(MIN_CACHE)]));
            }
            if plan.cache_budget.is_some() {
                let taken: usize = others.iter().map(|a| a.cache_reserved()).sum();
                let floor = if others.iter().any(|a| a.cache_reserved() == 0) { SHARED_FLOOR } else { 0 };
                let room = plan.cache_ceiling.saturating_sub(taken + floor);
                if c > room {
                    return Err(Refusal::new(
                        refused::CACHE_MORE_THAN_LEFT,
                        &[&mib(c), &app.name, &mib(plan.cache_ceiling), &mib(taken), &mib(floor), &mib(room)],
                    ));
                }
            }
        }
        if let Some(k) = kv
            && k < MIN_KV
        {
            return Err(Refusal::new(refused::KV_TOO_SMALL, &[&mib(MIN_KV)]));
        }
        // With it, what is set by hand on the whole server still fits the
        // memory; a change that lowers what does not fit is never refused.
        let now = self.by_hand(&self.all(), &plan);
        let mut then = self.by_hand(&others, &plan);
        then.apart += cache.unwrap_or(0);
        then.floor = if cache.is_none() || then.floor > 0 { SHARED_FLOOR } else { 0 };
        then.kv_own += kv.unwrap_or(0);
        then.kv_following += usize::from(kv.is_none());
        if let Err(refusal) = then.check(&format!("`{}`", app.name))
            && (now.check("").is_ok() || then.amount() > now.amount())
        {
            return Err(refusal);
        }
        // Kept first, in force once it is: a failure leaves everything as it was.
        self.save_record(app, &app.record_with(cache.unwrap_or(0), kv.unwrap_or(0)))
            .map_err(|e| Refusal::failed(&e))?;
        app.cache_reserved.store(cache.unwrap_or(0), Ordering::Relaxed);
        app.kv_fixed.store(kv.unwrap_or(0), Ordering::Relaxed);
        // The budget makes room for what is set apart, and everything is shared anew.
        self.make_room();
        let said = |bytes: Option<usize>| bytes.map_or("automatic".to_owned(), |b| format!("{} MiB", mib(b)));
        self.first.engine.event(
            "info",
            format!("memory of application `{}`: page cache {}, key-value store {}", app.name, said(cache), said(kv)),
        );
        Ok(())
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
            // Its part once it is there; `share_cache` then gives each its own.
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
        self.save_record(app, &app.record())
    }

    fn save_record(&self, app: &App, record: &Json) -> Result<()> {
        let mut key = REGISTRY_PREFIX.to_vec();
        key.extend_from_slice(app.name.as_bytes());
        self.first.engine.set_meta(&key, Some(record.to_string().as_bytes()))
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
            self.plan().kv_bytes,
        ));
        let token = add_token(&app, false);
        self.save(&app)?;
        self.others.write().unwrap_or_else(|e| e.into_inner()).insert(name.to_owned(), app.clone());
        // The budget stays what it is (the guard may have cut it): it is shared anew.
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
            cache_mb: Some(self.cache_reserved() as u64 >> 20).filter(|m| *m > 0),
            kv_mb: Some(self.kv_fixed() as u64 >> 20).filter(|m| *m > 0),
        }
    }
}
