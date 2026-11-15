//! Settings the owner changes while the server runs: the console's
//! Settings page, `/api/settings` and `nuage settings`.
//!
//! A setting has three possible sources, the first that speaks wins:
//!
//! 1. what the owner **saved** (console, API, command line): kept with the
//!    data, in the first application's metadata (encrypted, backed up and
//!    restored with it), so it follows the data wherever it goes;
//! 2. the **environment** (or the settings file): the variable of the same
//!    setting, as before;
//! 3. the **default**.
//!
//! A saved value is applied at once, with no restart, and "back to the
//! environment's value" removes it. `NUAGE_SETTINGS=environment` makes the
//! environment the only source: saved values are ignored and the console
//! shows the settings without changing them (for a server whose settings
//! are kept in a repository).
//!
//! Only settings that can change under a running server and cannot keep it
//! from starting are here. What the server needs before it can read its data
//! (the encryption key, the data directory), what it listens on (ports,
//! addresses, certificates) and what opens it (password, tokens) stay in the
//! environment: the console shows them, with where each comes from.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use memoire_orm::Error;

use super::ServerOptions;
use super::apps::MemoryPlan;
use super::i18n::{Lang, tf};
use crate::engine::Engine;

/// Why a setting is refused: a sentence of the console's dictionary, with
/// its figures. The console says it in its language; the API, the command
/// line and the events say it in English.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The sentence in English, with a `{}` for each figure.
    pub text: &'static str,
    /// The figures, in order.
    pub with: Vec<String>,
    /// Not a matter of value: this server takes no setting from here.
    pub forbidden: bool,
}

impl Refusal {
    pub(crate) fn new(text: &'static str, with: &[&dyn std::fmt::Display]) -> Refusal {
        Refusal { text, with: with.iter().map(|w| w.to_string()).collect(), forbidden: false }
    }

    /// Something else went wrong (the disk...): said as it is.
    pub(crate) fn failed(e: &Error) -> Refusal {
        Refusal { text: "{}", with: vec![e.to_string()], forbidden: false }
    }

    /// The sentence in the language `l`.
    pub(crate) fn said(&self, l: Lang) -> String {
        if self.text == "{}" {
            return self.with.concat();
        }
        let with: Vec<&dyn std::fmt::Display> = self.with.iter().map(|w| w as &dyn std::fmt::Display).collect();
        tf(l, self.text, &with)
    }
}

impl From<Refusal> for Error {
    fn from(r: Refusal) -> Error {
        if r.forbidden { Error::Unsupported(r.said(Lang::En)) } else { Error::Validation(r.said(Lang::En)) }
    }
}

/// The sentences of the refusals (each has its French and Spanish in the
/// console's dictionary: a test checks it).
pub(crate) mod refused {
    pub(crate) const TOO_MUCH_FOR_ANY: &str = "`{}`: {} MiB is more than any server has: a number alone is in MiB";
    pub(crate) const TOO_LITTLE: &str = "`{}`: {} MiB is too little: {} MiB at least, or `auto`";
    pub(crate) const NOT_AN_AMOUNT: &str =
        "`{}`: `{}` is not an amount of memory: write `auto`, `512 MiB` or `2 GiB` (a number alone is in MiB)";
    pub(crate) const NOT_BETWEEN: &str = "`{}`: {} is not between {} and {}";
    pub(crate) const NOT_A_NUMBER: &str = "`{}`: `{}` is not a whole number";
    pub(crate) const NOT_A_FLAG: &str = "`{}`: `{}` is not `true` or `false`";
    pub(crate) const NO_SUCH_SETTING: &str = "there is no setting `{}`: the settings are {}";
    pub(crate) const READ_ONLY: &str =
        "the settings of this server are those of its environment (NUAGE_SETTINGS=environment): change them there";
    pub(crate) const MORE_THAN_THE_SERVER: &str =
        "`memory.total_mb`: {} MiB is more than this server has ({} MiB): it would be killed for using it";
    pub(crate) const MEMORY_TOO_LITTLE: &str =
        "`memory.total_mb`: this server already uses {} MiB without its caches: {} MiB at least";
    pub(crate) const CACHE_UNDER_WHAT_IS_APART: &str = "`memory.cache_mb`: {} MiB is less than what is set apart for single databases ({} MiB, and {} MiB for the others to share): {} MiB at least, or lower their amounts first";
    pub(crate) const KV_ALL_TOO_LARGE: &str = "{}: the key-value stores would take {} MiB in all: more than a quarter of the {} MiB this server counts on ({} MiB at most)";
    pub(crate) const BY_HAND_TOO_MUCH: &str = "{}: {} MiB would be set by hand ({} MiB for the page caches, {} MiB for the key-value stores): more than three quarters of the {} MiB this server counts on ({} MiB at most)";
    pub(crate) const TOO_LITTLE_FOR_BY_HAND: &str = "`memory.total_mb`: {} MiB is too little for what is set by hand ({} MiB for the page caches, {} MiB for the key-value stores): {} MiB at least, or lower those amounts first";
    pub(crate) const PG_MORE_THAN_THREADS: &str = "`postgres.connections`: {} is more than this server can serve at once (each connection has a thread): {} at most";
    pub(crate) const CACHE_TOO_SMALL: &str = "a page cache needs {} MiB at least";
    pub(crate) const CACHE_MORE_THAN_LEFT: &str = "{} MiB for `{}` is more than is left: the page caches of this server may take {} MiB in all, {} MiB of it are set apart for other databases and {} MiB stay for those that share: {} MiB at most";
    pub(crate) const KV_TOO_SMALL: &str = "a key-value store needs {} MiB at least";
    pub(crate) const UNREADABLE: &str = "the saved settings were written by another version of Nuage DB and cannot be read by this one: nothing is saved over them (start the version that wrote them, or NUAGE_SETTINGS=environment)";
    pub(crate) const NOT_AN_ORIGIN: &str =
        "`{}` is not an origin: write `https://site.fr` (scheme and host, no path), or `*`";

    /// Every sentence above.
    #[cfg(test)]
    pub(crate) const ALL: [&str; 20] = [
        UNREADABLE,
        TOO_MUCH_FOR_ANY,
        TOO_LITTLE,
        NOT_AN_AMOUNT,
        NOT_BETWEEN,
        NOT_A_NUMBER,
        NOT_A_FLAG,
        NO_SUCH_SETTING,
        READ_ONLY,
        MORE_THAN_THE_SERVER,
        MEMORY_TOO_LITTLE,
        CACHE_UNDER_WHAT_IS_APART,
        KV_ALL_TOO_LARGE,
        BY_HAND_TOO_MUCH,
        TOO_LITTLE_FOR_BY_HAND,
        PG_MORE_THAN_THREADS,
        CACHE_TOO_SMALL,
        CACHE_MORE_THAN_LEFT,
        KV_TOO_SMALL,
        NOT_AN_ORIGIN,
    ];
}

/// The saved settings: one entry of the first application's metadata, a
/// JSON object of names and values, so that several saved together are
/// saved all or none.
const SAVED: &[u8] = b"setting:saved";

/// Seconds a statement may run when nobody said.
pub(crate) const STATEMENT_SECONDS: u64 = 60;

/// What a setting holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A number of MiB (`min` at least), or `auto`.
    Mib { min: u64 },
    /// A whole number between `min` and `max`.
    Count { min: u64, max: u64 },
    /// `true` or `false`.
    Flag,
}

/// A setting the owner may change while the server runs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Def {
    /// Its name in the API and on the command line (`memory.cache_mb`).
    pub key: &'static str,
    /// The variable of the environment that sets it (`NUAGE_CACHE_MB`).
    pub var: &'static str,
    pub kind: Kind,
}

/// Every setting, in the order the console shows them.
pub(crate) const DEFS: [Def; 7] = [
    Def { key: "memory.total_mb", var: "NUAGE_MEMORY_MB", kind: Kind::Mib { min: 64 } },
    Def { key: "memory.cache_mb", var: "NUAGE_CACHE_MB", kind: Kind::Mib { min: 16 } },
    Def { key: "memory.kv_mb", var: "NUAGE_KV_MB", kind: Kind::Mib { min: 1 } },
    Def { key: "statement.memory_mb", var: "NUAGE_STATEMENT_MB", kind: Kind::Mib { min: 8 } },
    Def { key: "statement.seconds", var: "NUAGE_STATEMENT_SECONDS", kind: Kind::Count { min: 0, max: 86_400 } },
    Def { key: "postgres.connections", var: "NUAGE_PG_MAX_CONNECTIONS", kind: Kind::Count { min: 1, max: 10_000 } },
    Def { key: "status.public", var: "NUAGE_PUBLIC_STATUS", kind: Kind::Flag },
];

/// The setting named `key`.
pub(crate) fn def(key: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.key == key)
}

/// What a setting is when nobody said anything, as it would be typed.
pub(crate) fn default_text(options: &ServerOptions, key: &str) -> String {
    match (key, def(key).map(|d| d.kind)) {
        ("statement.seconds", _) => STATEMENT_SECONDS.to_string(),
        ("postgres.connections", _) => options.pg_max_connections.to_string(),
        ("status.public", _) => options.public_status.to_string(),
        (_, Some(Kind::Mib { .. })) => "auto".into(),
        _ => String::new(),
    }
}

/// An amount of memory as its owner writes it, in MiB: a number alone
/// (`512`), or with its unit, in MiB (`512 MiB`, `512M`, `512 Mio`) or in GiB
/// (`2 GiB`, `2G`, `1.5 Go`, `1,5 Gio`).
pub(crate) fn parse_mib(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase().replace(',', ".");
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

impl Kind {
    /// Checks a value as its owner wrote it and gives it its one spelling
    /// (`AUTO` → `auto`, ` 512 ` → `512`, `2 GiB` → `2048`, `yes` → `true`).
    pub(crate) fn check(&self, key: &str, raw: &str) -> std::result::Result<String, Refusal> {
        let raw = raw.trim();
        match *self {
            Kind::Mib { min } => {
                if raw.eq_ignore_ascii_case("auto") {
                    return Ok("auto".into());
                }
                match parse_mib(raw) {
                    // More than any server has: a typing mistake (bytes for MiB).
                    Some(n) if n > (1 << 24) => Err(Refusal::new(refused::TOO_MUCH_FOR_ANY, &[&key, &n])),
                    Some(n) if n >= min => Ok(n.to_string()),
                    Some(n) => Err(Refusal::new(refused::TOO_LITTLE, &[&key, &n, &min])),
                    None => Err(Refusal::new(refused::NOT_AN_AMOUNT, &[&key, &raw])),
                }
            }
            Kind::Count { min, max } => match raw.parse::<u64>() {
                Ok(n) if (min..=max).contains(&n) => Ok(n.to_string()),
                Ok(n) => Err(Refusal::new(refused::NOT_BETWEEN, &[&key, &n, &min, &max])),
                Err(_) => Err(Refusal::new(refused::NOT_A_NUMBER, &[&key, &raw])),
            },
            Kind::Flag => match raw.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Ok("true".into()),
                "false" | "0" | "no" | "off" => Ok("false".into()),
                _ => Err(Refusal::new(refused::NOT_A_FLAG, &[&key, &raw])),
            },
        }
    }
}

/// Where the value of a setting comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Saved by the owner (console, API, command line).
    Saved,
    /// The environment, or the settings file.
    Environment,
    /// Nobody said: the default.
    Default,
}

impl Source {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Source::Saved => "saved",
            Source::Environment => "environment",
            Source::Default => "default",
        }
    }
}

/// The settings of a server: what its owner saved, over what the
/// environment says.
pub(crate) struct Settings {
    /// The first application's engine: the saved values are in its metadata.
    engine: Arc<Engine>,
    /// What the environment (or the settings file) says, by variable.
    /// `None`: a server built by hand, whose options say everything.
    env: Option<HashMap<String, String>>,
    saved: RwLock<BTreeMap<String, String>>,
    /// What was saved and is not in force, because the environment is the
    /// only source: shown, and removable.
    ignored: RwLock<BTreeMap<String, String>>,
    /// Saved settings this version does not understand (a later version's,
    /// or a value it no longer accepts): kept as they are, never in force.
    unknown: BTreeMap<String, serde_json::Value>,
    /// The saved settings could not be read at all (written by another
    /// version?): they are kept as they are, and nothing is saved over them.
    unreadable: bool,
    /// The environment is the only source (`NUAGE_SETTINGS=environment`).
    read_only: bool,
}

impl Settings {
    /// The settings kept in `engine`, over those of `env`.
    pub(crate) fn open(engine: Arc<Engine>, env: Option<HashMap<String, String>>, read_only: bool) -> Settings {
        let mut saved = BTreeMap::new();
        let mut unknown = BTreeMap::new();
        let mut unreadable = false;
        let kept = engine.meta_entries(SAVED).map(|entries| entries.into_iter().find(|(key, _)| key == SAVED));
        match kept {
            Ok(Some((_, value))) => match serde_json::from_slice::<BTreeMap<String, serde_json::Value>>(&value) {
                Ok(values) => {
                    for (key, value) in values {
                        // A value this version does not understand (a later version's,
                        // or one it no longer accepts) is kept aside, as it is, rather
                        // than stopping the server or being lost at the next save.
                        let checked = match (def(&key), value.as_str()) {
                            (Some(d), Some(text)) => Some(d.kind.check(&key, text)),
                            _ => None,
                        };
                        match checked {
                            Some(Ok(v)) => {
                                saved.insert(key, v);
                            }
                            Some(Err(e)) => {
                                engine.event("warn", format!("saved setting not in force: {}", e.said(Lang::En)));
                                unknown.insert(key, value);
                            }
                            None => {
                                unknown.insert(key, value);
                            }
                        }
                    }
                }
                Err(e) => {
                    engine.event(
                        "error",
                        format!(
                            "the saved settings cannot be read (written by another version?): none is in force, and they are kept as they are: {e}"
                        ),
                    );
                    unreadable = true;
                }
            },
            Ok(None) => {}
            Err(e) => {
                engine.event("error", format!("cannot read the saved settings: {e}"));
                unreadable = true;
            }
        }
        let mut ignored = BTreeMap::new();
        if read_only && !saved.is_empty() {
            engine.event(
                "info",
                format!(
                    "NUAGE_SETTINGS=environment: the {} setting(s) saved from the console are ignored",
                    saved.len()
                ),
            );
            ignored = std::mem::take(&mut saved);
        }
        Settings {
            engine,
            env,
            saved: RwLock::new(saved),
            ignored: RwLock::new(ignored),
            unknown,
            unreadable,
            read_only,
        }
    }

    /// What was saved and is not in force (`NUAGE_SETTINGS=environment`):
    /// the page shows it, and lets its owner remove it.
    pub(crate) fn ignored(&self) -> Vec<(String, String)> {
        self.ignored.read().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// The saved values that win over a different value of the environment,
    /// one sentence each: said at startup, so that a variable changed on the
    /// host and seemingly ignored is explained.
    pub(crate) fn overrides(&self) -> Vec<String> {
        DEFS.iter()
            .filter_map(|d| {
                let (saved, env) = (self.saved(d.key)?, self.environment(d.key)?);
                let same = d.kind.check(d.key, &env).is_ok_and(|e| e == saved);
                // A saved memory only lowers the environment's.
                if let Some(lower) = self.memory_capped().filter(|_| d.key == "memory.total_mb") {
                    return Some(format!(
                        "{}={env} is under the memory saved from the console ({saved} MiB): {lower} MiB is in force \
                         (a saved memory only lowers the environment's)",
                        d.var
                    ));
                }
                (!same).then(|| {
                    format!(
                        "{}={env} is set in the environment, but the value saved from the console ({saved}) is the one \
                         in force: the Settings page gives it back to the environment",
                        d.var
                    )
                })
            })
            .collect()
    }

    /// The environment is the only source: nothing can be saved.
    pub(crate) fn read_only(&self) -> bool {
        self.read_only
    }

    /// What the owner saved for `key`.
    pub(crate) fn saved(&self, key: &str) -> Option<String> {
        self.saved.read().unwrap_or_else(|e| e.into_inner()).get(key).cloned()
    }

    /// What the environment says for `key` (unchecked: as it was written).
    pub(crate) fn environment(&self, key: &str) -> Option<String> {
        let d = def(key)?;
        self.env.as_ref()?.get(d.var).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
    }

    /// The value in force and where it comes from (`None`: the default).
    pub(crate) fn value(&self, key: &str) -> (Option<String>, Source) {
        if let Some(v) = self.saved(key) {
            return (Some(v), Source::Saved);
        }
        match self.environment(key) {
            Some(v) => (Some(v), Source::Environment),
            None => (None, Source::Default),
        }
    }

    /// What was said for `key`, for a budget to be computed from it:
    /// `Some(value)` (`None` inside: nothing said, the default), or `None`
    /// for a server built by hand and nothing saved, whose options stand.
    fn said(&self, key: &str) -> Option<Option<String>> {
        match (self.saved(key), &self.env) {
            (Some(v), _) => Some(Some(v)),
            (None, Some(_)) => Some(self.environment(key)),
            (None, None) => None,
        }
    }

    /// Checks that `value` may be saved for `key`, and gives it its one
    /// spelling (`None`: what was saved is to be removed).
    pub(crate) fn check(&self, key: &str, value: Option<&str>) -> std::result::Result<Option<String>, Refusal> {
        let Some(d) = def(key) else {
            let known: Vec<&str> = DEFS.iter().map(|d| d.key).collect();
            return Err(Refusal::new(refused::NO_SUCH_SETTING, &[&key, &known.join(", ")]));
        };
        // Nothing is saved while the environment rules; what was saved before
        // can still be removed.
        if self.read_only && value.is_some() {
            return Err(Refusal { forbidden: true, ..Refusal::new(refused::READ_ONLY, &[]) });
        }
        value.map(|v| d.kind.check(key, v)).transpose()
    }

    /// Saves several settings at once, all or none (each checked first): a
    /// value, or `None` to remove what was saved, so that the environment's
    /// value, or the default, is in force again. The caller puts them in
    /// force ([`super::State::save_settings`] does both).
    pub(crate) fn save(&self, changes: &[(&str, Option<&str>)]) -> std::result::Result<bool, Refusal> {
        if self.unreadable {
            return Err(Refusal::new(refused::UNREADABLE, &[]));
        }
        let mut checked = Vec::new();
        for (key, value) in changes {
            checked.push((*key, self.check(key, *value)?));
        }
        // Where the environment rules, what was saved is not in force: it is
        // what can be removed.
        let kept = if self.read_only { &self.ignored } else { &self.saved };
        let mut values = kept.read().unwrap_or_else(|e| e.into_inner()).clone();
        let before = values.clone();
        for (key, value) in &checked {
            match value {
                Some(v) => values.insert((*key).to_owned(), v.clone()),
                None => values.remove(*key),
            };
        }
        if values == before {
            return Ok(false);
        }
        let mut entry: BTreeMap<&str, serde_json::Value> =
            self.unknown.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        entry.extend(values.iter().map(|(k, v)| (k.as_str(), serde_json::Value::String(v.clone()))));
        let json = serde_json::to_vec(&entry).unwrap_or_default();
        self.engine.set_meta(SAVED, (!entry.is_empty()).then_some(json.as_slice())).map_err(|e| Refusal::failed(&e))?;
        *kept.write().unwrap_or_else(|e| e.into_inner()) = values;
        for (key, value) in &checked {
            self.engine.event(
                "info",
                match value {
                    Some(v) => format!("setting `{key}` saved: {v}"),
                    None => format!(
                        "setting `{key}`: what was saved is removed, the environment's value or the default is in force"
                    ),
                },
            );
        }
        Ok(true)
    }

    /// Says the memory to count on to [`crate::memory`]: the value in force
    /// (saved, else the environment's or the settings file's), or with none
    /// the memory the system reports. A server built by hand and nothing
    /// saved: `NUAGE_MEMORY_MB` as before.
    pub(crate) fn declare_memory(&self) {
        match self.said("memory.total_mb") {
            Some(Some(said)) => match said.parse::<u64>() {
                Ok(mib) => crate::memory::declare(Some(mib.min(self.memory_cap().unwrap_or(mib)) << 20)),
                // `auto`: what the system reports, under the environment's figure
                // when it gives one (a host that limits the memory without saying so).
                Err(_) => crate::memory::declare(self.environment_memory().map(|mib| mib << 20)),
            },
            Some(None) => crate::memory::declare(None),
            None => crate::memory::follow_environment(),
        }
    }

    /// The most a saved `memory.total_mb` may say, in MiB, besides what the
    /// system reports: the figure of the environment, set for a host that
    /// limits the memory without saying so. Saving is for lowering it.
    pub(crate) fn memory_cap(&self) -> Option<u64> {
        self.saved("memory.total_mb")?;
        self.environment("memory.total_mb")?.parse().ok()
    }

    /// The memory in force, in MiB, when it is under the one saved: the
    /// environment's figure is lower (see [`Settings::declare_memory`]).
    pub(crate) fn memory_capped(&self) -> Option<u64> {
        let saved: u64 = self.saved("memory.total_mb")?.parse().ok()?;
        self.memory_cap().filter(|cap| *cap < saved)
    }

    /// The figure of the environment for the memory, in MiB, when it gives one.
    pub(crate) fn environment_memory(&self) -> Option<u64> {
        self.environment("memory.total_mb")?.parse().ok()
    }

    /// The budgets of the page caches and of the key-value stores, from the
    /// settings and the memory there is now. A server built by hand keeps
    /// what its options say for what nobody saved.
    pub(crate) fn memory_plan(&self, options: &ServerOptions) -> MemoryPlan {
        use crate::memory;
        let (cache_budget, cache_ceiling) = match self.said("memory.cache_mb") {
            Some(said) => {
                let mut budget =
                    memory::cache_bytes(said.as_deref()).or_else(|_| memory::cache_bytes(None)).unwrap_or(64 << 20);
                // Saved on a larger server, or before the memory was lowered:
                // never more than three quarters of the memory there is now.
                if self.saved("memory.cache_mb").is_some()
                    && let Some(total) = memory::MemoryInfo::now().total_bytes
                {
                    budget = budget.min((total / 4 * 3) as usize);
                }
                (Some(budget), memory::cache_ceiling(said.as_deref(), budget))
            }
            None => (options.cache_budget, options.cache_ceiling.unwrap_or(0)),
        };
        let (kv_bytes, kv_budget) = match self.said("memory.kv_mb") {
            Some(said) => (
                memory::kv_bytes(said.as_deref()).or_else(|_| memory::kv_bytes(None)).unwrap_or(32 << 20),
                memory::kv_budget(said.as_deref()),
            ),
            None => (options.kv_bytes, options.kv_budget),
        };
        // All the key-value memory, own amounts included: a quarter of the
        // memory (a server built by hand keeps what its options say).
        let kv_room = self
            .said("memory.kv_mb")
            .and_then(|_| memory::MemoryInfo::now().total_bytes)
            .map(|total| (total / 4) as usize);
        let kv_by_hand = match self.said("memory.kv_mb") {
            Some(said) => said.is_some_and(|v| !v.eq_ignore_ascii_case("auto")),
            None => options.kv_budget.is_none(),
        };
        MemoryPlan { kv_bytes, kv_budget, kv_by_hand, kv_room, cache_budget, cache_ceiling }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What this version does not understand is kept as it is, and saved
    /// back untouched; saved settings it cannot read at all are never
    /// overwritten.
    #[test]
    fn what_another_version_saved_is_kept() {
        let dir = std::env::temp_dir().join(format!("nuage-settings-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let options = crate::engine::EngineOptions { sync: false, ..Default::default() };
        let engine = Engine::open(&dir, options).unwrap();
        let later = br#"{"status.public":"true","future.thing":{"a":1},"postgres.connections":"99999"}"#;
        engine.set_meta(SAVED, Some(later)).unwrap();
        let settings = Settings::open(engine.clone(), Some(HashMap::new()), false);
        assert_eq!(settings.saved("status.public").as_deref(), Some("true"));
        assert_eq!(settings.saved("postgres.connections"), None, "not accepted by this version: not in force");
        assert!(settings.save(&[("memory.kv_mb", Some("8"))]).unwrap());
        let kept = engine.meta_entries(SAVED).unwrap().pop().unwrap().1;
        let kept: serde_json::Value = serde_json::from_slice(&kept).unwrap();
        assert_eq!(
            kept,
            serde_json::json!({
                "status.public": "true", "future.thing": {"a": 1}, "postgres.connections": "99999", "memory.kv_mb": "8"
            })
        );
        assert!(!settings.save(&[("memory.kv_mb", Some("8"))]).unwrap(), "nothing changed");
        // Unreadable: nothing is saved over it.
        engine.set_meta(SAVED, Some(b"not json")).unwrap();
        let settings = Settings::open(engine.clone(), Some(HashMap::new()), false);
        assert_eq!(settings.save(&[("status.public", Some("true"))]).unwrap_err().text, refused::UNREADABLE);
        assert_eq!(engine.meta_entries(SAVED).unwrap().pop().unwrap().1, b"not json");
        drop((settings, engine));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn values_are_checked_and_spelled_one_way() {
        let mib = Kind::Mib { min: 16 };
        assert_eq!(mib.check("memory.cache_mb", " 512 ").unwrap(), "512");
        assert_eq!(mib.check("memory.cache_mb", "AUTO").unwrap(), "auto");
        // An amount is written with its unit, in MiB or in GiB, as each language writes them.
        for (written, mib) in [
            ("512 MiB", 512),
            ("512M", 512),
            ("512 Mio", 512),
            ("2 GiB", 2048),
            ("2G", 2048),
            ("2 Go", 2048),
            ("1.5 GiB", 1536),
            ("1,5 Gio", 1536),
            ("0.25g", 256),
        ] {
            assert_eq!(parse_mib(written), Some(mib), "{written}");
        }
        assert_eq!(mib.check("memory.cache_mb", "1,5 Go").unwrap(), "1536");
        for bad in ["8", "-1", "1.5", "512 KB", "2 T", "", "99999999999", "1e3", "0.001 G"] {
            assert!(mib.check("memory.cache_mb", bad).is_err(), "{bad}");
        }
        let count = Kind::Count { min: 1, max: 100 };
        assert_eq!(count.check("postgres.connections", "100").unwrap(), "100");
        assert!(
            count.check("postgres.connections", "0").is_err() && count.check("postgres.connections", "101").is_err()
        );
        assert_eq!(Kind::Flag.check("status.public", "Yes").unwrap(), "true");
        assert_eq!(Kind::Flag.check("status.public", "off").unwrap(), "false");
        assert!(Kind::Flag.check("status.public", "maybe").is_err());
        // A refusal is said in the console's three languages.
        for text in refused::ALL {
            assert!(crate::server::i18n::translated(text), "no translation: {text}");
        }
        let refusal = mib.check("memory.cache_mb", "8").unwrap_err();
        assert_eq!(refusal.said(Lang::En), "`memory.cache_mb`: 8 MiB is too little: 16 MiB at least, or `auto`");
        assert_eq!(refusal.said(Lang::Fr), "`memory.cache_mb` : 8 Mio, c'est trop peu : 16 Mio au moins, ou `auto`");
        // Every setting has a name and a variable of its own.
        let mut keys: Vec<&str> = DEFS.iter().map(|d| d.key).collect();
        let mut vars: Vec<&str> = DEFS.iter().map(|d| d.var).collect();
        keys.sort_unstable();
        keys.dedup();
        vars.sort_unstable();
        vars.dedup();
        assert_eq!((keys.len(), vars.len()), (DEFS.len(), DEFS.len()));
    }
}
