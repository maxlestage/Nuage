//! Configuration of the `nuage` binary, from environment variables.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `PORT` | port to listen on (set by Heroku and most hosts) | `8080` |
//! | `NUAGE_BIND` | address to listen on (`127.0.0.1`: only this machine, behind a proxy such as Caddy) | `0.0.0.0` |
//! | `NUAGE_PG_PORT` | also serve the PostgreSQL protocol on this port (`5432`): every language's PostgreSQL driver connects, with a token as its password | off |
//! | `NUAGE_PG_BIND` | address of the PostgreSQL protocol (`0.0.0.0` while the website stays behind a proxy on `127.0.0.1`) | `NUAGE_BIND` |
//! | `NUAGE_TLS_CERT`, `NUAGE_TLS_KEY` | PEM files of a certificate and its key: TLS for the PostgreSQL protocol (`sslmode=require`) and the key-value port | none: in clear |
//! | `NUAGE_KV_PORT` | also serve the key-value store on a port of its own (`6379`), over the protocol Redis clients speak: they connect to an application's store, with a token as their password | off |
//! | `NUAGE_KV_BIND` | address of the key-value port | `NUAGE_BIND` |
//! | `NUAGE_KV_MB` | memory of each application's key-value store (`/v1/kv`, the key-value port), in MiB, or `auto`; past it, the least recently used keys go. Not used with `NUAGE_KV_STORAGE=database` | `auto`: a sixteenth of the memory (4 to 256 MiB) each, and an eighth of it for all of them together |
//! | `NUAGE_KV_STORAGE` | where every application's key-value store is kept: `memory` (in the server's memory: the fastest; a restart empties it) or `database` (in the application's database, on disk: it survives restarts and is in the backups; a write waits for the disk like a row; nothing is evicted, so give cache entries an expiry; a value is 64 MiB at most). Going back to `memory` leaves the keys in the database, found again when `database` comes back. Any other value stops the start | `memory` |
//! | `NUAGE_DATA_DIR` | data directory | `nuage-data` |
//! | `NUAGE_NAME` | name of the database | `nuage` |
//! | `NUAGE_ENCRYPTION_KEY` | the key everything is encrypted with: 64 hex digits (`openssl rand -hex 32`), base64 of 32 bytes, or a passphrase | none: the binary refuses to start |
//! | `NUAGE_TOKEN` | token of the first application (named `NUAGE_NAME`); the console creates the others | none: that application has no token |
//! | `NUAGE_ADMIN_PASSWORD` | password of the console (`/admin`) | none: the console is off |
//! | `NUAGE_ADMIN_TOKEN` | token of the management API (`/api/apps`, `nuage apps`): creates applications and their tokens; 32 characters at least (`nuage genkey`) | none: the API is off |
//! | `NUAGE_PUBLIC_STATUS` | `true`: `/api/status` shows a public subset without credentials | `false` |
//! | `NUAGE_CACHE_MB` | memory the page caches of all the applications may hold together, in MiB, or `auto`; shared equally among them | `auto`: it follows the memory: a quarter of it at first, more while there is room (up to six tenths), less when it gets tight |
//! | `NUAGE_MEMORY_MB` | the memory to count on, in MiB, for a host that limits it without saying so | the container's limit, else the machine's memory |
//! | `NUAGE_PG_MAX_CONNECTIONS` | PostgreSQL connections served at once (each has a thread) | `100`, less where the system allows few threads |
//! | `NUAGE_ENV_FILE` | read the settings from this file too (`KEY=value` lines; `nuage --env-file <file>` does the same); a variable of the environment wins over the file | none |
//! | `NUAGE_SETTINGS` | `environment`: the settings are those of the environment only; what was saved from the console (Settings page, `/api/settings`, `nuage settings`) is ignored and the console changes nothing | saved settings win over the environment |
//! | `NUAGE_STATEMENT_MB` | memory one SQL statement may hold at once (the rows it sorts, joins or returns), in MiB, or `auto`; past it the statement fails with a clear error | `auto`: an eighth of the memory (32 MiB to 2 GiB) |
//! | `NUAGE_STATEMENT_SECONDS` | seconds one statement may run before it is stopped and its changes undone; `0`: no limit | `60` |
//! | `NUAGE_BACKUP_DIR` | back up to this directory (another disk: a backup on the data's disk dies with it) | |
//! | `NUAGE_BACKUP_S3_BUCKET` | back up to this S3 bucket | |
//! | `NUAGE_BACKUP_S3_ENDPOINT` | S3 endpoint URL | `https://s3.<region>.amazonaws.com` |
//! | `NUAGE_BACKUP_S3_REGION` | S3 region | `us-east-1` |
//! | `NUAGE_BACKUP_S3_PREFIX` | prefix of the backup objects | `nuage/` |
//! | `NUAGE_BACKUP_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP_S3_SECRET_ACCESS_KEY` | S3 credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
//! | `NUAGE_BACKUP_SNAPSHOT_MINUTES` | minutes between two snapshots (of each destination) | `60` |
//! | `NUAGE_BACKUP2_DIR` | a second backup destination, kept in step with the first: this folder. Each destination gets the snapshots and the log with its own progress, on a thread of its own: one that does not answer, the first or the second, never holds the other back, and gets a whole snapshot when it answers again; the stop sends to both at once, the first for as long as it takes, the second for 20 seconds at most. An empty data directory is restored from one destination, whole: the one that reaches furthest, the first on a tie; a destination that does not answer (a second that has not answered 5 seconds after the first), or two destinations that hold different databases, stop the start. Set alone, it is the only destination. Set with `NUAGE_BACKUP2_S3_BUCKET`, or naming the same place as the first, it stops the start. To move the backups without a stop: add the second, wait until the console shows each application's last backup with no error (it shows none until the second has a whole snapshot of each), swap the two, then remove the old one, now the second | none |
//! | `NUAGE_BACKUP2_S3_BUCKET` | the second destination in this S3 bucket (see `NUAGE_BACKUP2_DIR`); one of the other `NUAGE_BACKUP2_S3_*` set without it stops the start | |
//! | `NUAGE_BACKUP2_S3_ENDPOINT` | its endpoint URL (never the first's: set it for storage other than AWS) | `https://s3.<region>.amazonaws.com` |
//! | `NUAGE_BACKUP2_S3_REGION` | its region (never the first's) | `us-east-1` |
//! | `NUAGE_BACKUP2_S3_PREFIX` | its prefix (never the first's) | `nuage/` |
//! | `NUAGE_BACKUP2_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP2_S3_SECRET_ACCESS_KEY` | its credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
//! | `NUAGE_RESTORE_REQUIRED` | `true`: an empty data directory must be restored from the backup; when the backup holds nothing to restore, or no backup is set, the server refuses to start, and creates and writes nothing. A registered application whose directory is empty and whose backup holds nothing is not served either: the server starts without it and lists it as one that cannot be opened (complete its backup and restart, or delete it in the console); a new application starts empty. For a disk erased at each restart (Heroku), once the server holds data: a first start has nothing to restore. `1`, `yes`, `0`, `no` too; any other value stops the start | `false`: an empty start is allowed, and said |
//! | `NUAGE_FILES_S3_BUCKET` | keep the applications' files (`/v1/files`) in this S3-compatible bucket (Cloudflare R2, AWS S3, MinIO...); the bytes go straight between the clients and the bucket, through signed links | none: no files |
//! | `NUAGE_FILES_S3_ENDPOINT` | its endpoint URL (`https://<account>.r2.cloudflarestorage.com` for R2) | `https://s3.<region>.amazonaws.com` |
//! | `NUAGE_FILES_S3_REGION` | its region | `auto` on R2, else `us-east-1` |
//! | `NUAGE_FILES_S3_PREFIX` | prefix of the objects; each application's files go under `<prefix><application>/` | `files/` |
//! | `NUAGE_FILES_S3_ACCESS_KEY_ID`, `NUAGE_FILES_S3_SECRET_ACCESS_KEY` | its credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
//! | `NUAGE_FILES_LINK_MINUTES` | how long a download or upload link works (10080 at most: 7 days) | `60` |
//! | `NUAGE_KEEP_AWAKE_URL` | the server requests this address (its own `/api/health`) so a host that puts idle servers to sleep (Heroku Eco) keeps it awake | off |
//! | `NUAGE_KEEP_AWAKE_MINUTES` | minutes between two requests | `20` |
//! | `NUAGE_KEEP_AWAKE_HOURS` | only in these hours, UTC (`6-22`) | always |

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use memoire_orm::{Error, Result};

use crate::crypto::Encryption;
use crate::engine::{BackupOptions, BackupStore, DirectoryStore, EngineOptions, MirrorStore};
#[cfg(feature = "s3")]
use crate::s3::{S3Options, S3Store};
#[cfg(feature = "server")]
use crate::server::ServerOptions;

/// Everything the binary needs (feature `server`).
#[cfg(feature = "server")]
#[derive(Debug)]
pub struct Config {
    /// Port to listen on.
    pub port: u16,
    /// Address to listen on.
    pub bind: String,
    /// Port of the PostgreSQL protocol, if it is on.
    pub pg_port: Option<u16>,
    /// Address of the PostgreSQL protocol.
    pub pg_bind: String,
    /// Port of the key-value store, if it has one of its own.
    pub kv_port: Option<u16>,
    /// Address of the key-value port.
    pub kv_bind: String,
    /// Certificate and key (PEM files) for TLS on the PostgreSQL protocol.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Engine settings.
    pub engine: EngineOptions,
    /// Server settings.
    pub server: ServerOptions,
    /// Requests to its own address, so the host does not put it to sleep.
    pub keep_awake: Option<crate::keepawake::KeepAwake>,
    /// The backup folder (`NUAGE_BACKUP_DIR`), if the backups go to one.
    pub backup_dir: Option<PathBuf>,
    /// The backup destinations, in order: the start of their variables'
    /// names (`NUAGE_BACKUP`, `NUAGE_BACKUP2`), and the folder of one that is
    /// a folder (`None`: a bucket). Empty without a backup.
    pub backups: Vec<(&'static str, Option<PathBuf>)>,
    /// An empty data directory must be restored from the backup
    /// (`NUAGE_RESTORE_REQUIRED`): the binary opens it with
    /// [`Engine::open_existing`](crate::Engine::open_existing), and the
    /// registered applications follow it.
    pub restore_required: bool,
}

/// The data directory and the engine settings, through `var`: `NUAGE_DATA_DIR`,
/// `NUAGE_NAME`, `NUAGE_ENCRYPTION_KEY`, `NUAGE_CACHE_MB`, `NUAGE_KV_STORAGE`,
/// and the backups (`NUAGE_BACKUP_DIR`, or `NUAGE_BACKUP_S3_*` with the feature
/// `s3`; a second destination kept in step, `NUAGE_BACKUP2_DIR` or
/// `NUAGE_BACKUP2_S3_*`, then a [`MirrorStore`];
/// `NUAGE_BACKUP_SNAPSHOT_MINUTES`). What an embedded database needs, without
/// the server.
pub fn engine_from_vars(var: impl Fn(&str) -> Option<String>) -> Result<(PathBuf, EngineOptions)> {
    let get = |k: &str| var(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let bad = |k: &str, v: &str| Error::Validation(format!("{k}=`{v}` is not valid"));
    let mut engine = EngineOptions::default();
    if let Some(name) = get("NUAGE_NAME") {
        engine.name = name;
    }
    if let Some(secret) = get("NUAGE_ENCRYPTION_KEY") {
        engine.encryption = Some(Encryption::parse(&secret)?);
    }
    // One budget for the caches: a server shares it among its applications
    // (the first one opens with it all); an embedded database has it whole.
    engine.cache_bytes = crate::memory::cache_bytes(get("NUAGE_CACHE_MB").as_deref()).map_err(Error::Validation)?;
    // A value mistyped must not mean memory: the keys would seem lost.
    engine.kv_in_database = match get("NUAGE_KV_STORAGE") {
        None => false,
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "memory" => false,
            "database" => true,
            _ => return Err(Error::Validation(format!("NUAGE_KV_STORAGE=`{v}`: `memory` or `database`"))),
        },
    };
    // A second destination kept in step with the first; set alone, it is the
    // only one.
    let store = match (backup_store(&get, FIRST)?, backup_store(&get, SECOND)?) {
        (Some((one, first)), Some((two, second))) => {
            if one.same_as(&two) {
                return Err(Error::Validation(format!(
                    "the two backup destinations are the same place ({}): set NUAGE_BACKUP2_* to another folder or \
                     bucket, or remove them",
                    first.describe()
                )));
            }
            Some(Arc::new(MirrorStore::new(first, second)) as Arc<dyn BackupStore>)
        }
        (Some((_, only)), None) | (None, Some((_, only))) => Some(only),
        (None, None) => None,
    };
    if let Some(store) = store {
        let mut backup = BackupOptions::new(store);
        if let Some(m) = get("NUAGE_BACKUP_SNAPSHOT_MINUTES") {
            backup.snapshot_every =
                Duration::from_secs(m.parse::<u64>().map_err(|_| bad("NUAGE_BACKUP_SNAPSHOT_MINUTES", &m))? * 60);
        }
        engine.backup = Some(backup);
    }
    let data_dir = get("NUAGE_DATA_DIR").map_or_else(|| PathBuf::from("nuage-data"), PathBuf::from);
    Ok((data_dir, engine))
}

/// The variables of the first backup destination start with this.
const FIRST: &str = "NUAGE_BACKUP";
/// ... and those of the second, kept in step with it.
const SECOND: &str = "NUAGE_BACKUP2";

/// Where a backup destination of the settings is, to tell whether two are the
/// same place.
#[derive(Debug)]
enum Place {
    Folder(PathBuf),
    /// Endpoint, bucket and prefix.
    #[cfg_attr(not(feature = "s3"), allow(dead_code))]
    Bucket(String, String, String),
}

impl Place {
    /// The same folder (written two ways, or through a link), or the same
    /// prefix of the same bucket at the same endpoint. A prefix is used as it
    /// is written: `nuage` and `nuage/` are two places.
    fn same_as(&self, other: &Place) -> bool {
        match (self, other) {
            (Place::Folder(a), Place::Folder(b)) => {
                a.components().eq(b.components())
                    || matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
            }
            (Place::Bucket(e1, b1, p1), Place::Bucket(e2, b2, p2)) => {
                e1.trim_end_matches('/').eq_ignore_ascii_case(e2.trim_end_matches('/')) && b1 == b2 && p1 == p2
            }
            _ => false,
        }
    }
}

/// The backup destination of the variables that start with `base`
/// (`NUAGE_BACKUP` or `NUAGE_BACKUP2`): `<base>_DIR`, or `<base>_S3_BUCKET`
/// and the other `<base>_S3_*`. For the first, a folder wins over a bucket,
/// as it always has; for the second, both are refused.
fn backup_store(get: &impl Fn(&str) -> Option<String>, base: &str) -> Result<Option<(Place, Arc<dyn BackupStore>)>> {
    match (get(&format!("{base}_DIR")), get(&format!("{base}_S3_BUCKET"))) {
        (Some(_), Some(_)) if base != FIRST => {
            Err(Error::Validation(format!("set {base}_DIR or {base}_S3_BUCKET, not both")))
        }
        (Some(dir), _) => {
            let store: Arc<dyn BackupStore> = Arc::new(DirectoryStore::new(&dir));
            Ok(Some((Place::Folder(PathBuf::from(dir)), store)))
        }
        (None, Some(bucket)) => s3_store(get, base, bucket).map(Some),
        (None, None) if base != FIRST => {
            // The bucket mistyped or left out: the second's other settings say a
            // destination was meant, and starting without it would end a move of
            // the backups with none (these names are new: refusing them changes
            // nothing for a server that sets none).
            let stray = ["ENDPOINT", "REGION", "PREFIX", "ACCESS_KEY_ID", "SECRET_ACCESS_KEY"]
                .into_iter()
                .find(|name| get(&format!("{base}_S3_{name}")).is_some());
            match stray {
                Some(name) => Err(Error::Validation(format!(
                    "{base}_S3_{name} is set but {base}_S3_BUCKET is not: set it (or {base}_DIR), or remove the \
                     {base}_* settings"
                ))),
                None => Ok(None),
            }
        }
        (None, None) => Ok(None),
    }
}

#[cfg(feature = "s3")]
fn s3_store(
    get: &impl Fn(&str) -> Option<String>,
    base: &str,
    bucket: String,
) -> Result<(Place, Arc<dyn BackupStore>)> {
    let var = |name: &str| get(&format!("{base}_S3_{name}"));
    let region = var("REGION").unwrap_or_else(|| "us-east-1".into());
    let key = var("ACCESS_KEY_ID")
        .or_else(|| get("AWS_ACCESS_KEY_ID"))
        .ok_or_else(|| Error::Validation(format!("{base}_S3_ACCESS_KEY_ID is missing")))?;
    let secret = var("SECRET_ACCESS_KEY")
        .or_else(|| get("AWS_SECRET_ACCESS_KEY"))
        .ok_or_else(|| Error::Validation(format!("{base}_S3_SECRET_ACCESS_KEY is missing")))?;
    let options = S3Options {
        endpoint: var("ENDPOINT").unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com")),
        bucket,
        region,
        prefix: var("PREFIX").unwrap_or_else(|| "nuage/".into()),
        access_key_id: key,
        secret_access_key: secret,
    };
    let place = Place::Bucket(options.endpoint.clone(), options.bucket.clone(), options.prefix.clone());
    Ok((place, Arc::new(S3Store::new(options)?)))
}

/// Without the feature `s3`, S3 backups are refused rather than silently ignored.
#[cfg(not(feature = "s3"))]
fn s3_store(
    _get: &impl Fn(&str) -> Option<String>,
    base: &str,
    _bucket: String,
) -> Result<(Place, Arc<dyn BackupStore>)> {
    Err(Error::Validation(format!("{base}_S3_BUCKET is set, but this build of Nuage DB has no `s3` feature")))
}

/// The bucket of `/v1/files`, from `NUAGE_FILES_S3_*`.
#[cfg(feature = "server")]
fn file_store(get: &impl Fn(&str) -> Option<String>, bucket: String) -> Result<crate::server::FileStore> {
    let endpoint = get("NUAGE_FILES_S3_ENDPOINT");
    let on_r2 = endpoint.as_deref().is_some_and(|e| e.contains(".r2.cloudflarestorage.com"));
    let region = get("NUAGE_FILES_S3_REGION").unwrap_or_else(|| if on_r2 { "auto" } else { "us-east-1" }.into());
    let key = get("NUAGE_FILES_S3_ACCESS_KEY_ID").or_else(|| get("AWS_ACCESS_KEY_ID")).ok_or_else(|| {
        Error::Validation("NUAGE_FILES_S3_BUCKET is set: NUAGE_FILES_S3_ACCESS_KEY_ID is missing".into())
    })?;
    let secret = get("NUAGE_FILES_S3_SECRET_ACCESS_KEY").or_else(|| get("AWS_SECRET_ACCESS_KEY")).ok_or_else(|| {
        Error::Validation("NUAGE_FILES_S3_BUCKET is set: NUAGE_FILES_S3_SECRET_ACCESS_KEY is missing".into())
    })?;
    let minutes = match get("NUAGE_FILES_LINK_MINUTES") {
        Some(m) => m
            .parse::<u64>()
            .ok()
            .filter(|m| (1..=7 * 24 * 60).contains(m))
            .ok_or_else(|| Error::Validation(format!("NUAGE_FILES_LINK_MINUTES=`{m}`: 1 to 10080 minutes")))?,
        None => 60,
    };
    let mut prefix = get("NUAGE_FILES_S3_PREFIX").unwrap_or_else(|| "files/".into());
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    crate::server::FileStore::new(
        S3Options {
            endpoint: endpoint.unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com")),
            bucket,
            region,
            prefix,
            access_key_id: key,
            secret_access_key: secret,
        },
        minutes * 60,
    )
}

/// The settings read once at startup, in the order the console shows them,
/// and whether each is a secret (shown as set or not, never its value).
#[cfg(feature = "server")]
const STARTUP: [(&str, bool); 34] = [
    ("PORT", false),
    ("NUAGE_BIND", false),
    ("NUAGE_DATA_DIR", false),
    ("NUAGE_NAME", false),
    ("NUAGE_ENCRYPTION_KEY", true),
    ("NUAGE_TOKEN", true),
    ("NUAGE_ADMIN_PASSWORD", true),
    ("NUAGE_ADMIN_TOKEN", true),
    ("NUAGE_PG_PORT", false),
    ("NUAGE_PG_BIND", false),
    ("NUAGE_KV_PORT", false),
    ("NUAGE_KV_BIND", false),
    ("NUAGE_TLS_CERT", false),
    ("NUAGE_TLS_KEY", false),
    ("NUAGE_BACKUP_DIR", false),
    ("NUAGE_BACKUP_S3_BUCKET", false),
    ("NUAGE_BACKUP_S3_ENDPOINT", false),
    ("NUAGE_BACKUP_S3_REGION", false),
    ("NUAGE_BACKUP_S3_PREFIX", false),
    ("NUAGE_BACKUP_S3_ACCESS_KEY_ID", true),
    ("NUAGE_BACKUP_S3_SECRET_ACCESS_KEY", true),
    ("NUAGE_BACKUP_SNAPSHOT_MINUTES", false),
    ("NUAGE_FILES_S3_BUCKET", false),
    ("NUAGE_FILES_S3_ENDPOINT", false),
    ("NUAGE_FILES_S3_REGION", false),
    ("NUAGE_FILES_S3_PREFIX", false),
    ("NUAGE_FILES_S3_ACCESS_KEY_ID", true),
    ("NUAGE_FILES_S3_SECRET_ACCESS_KEY", true),
    ("NUAGE_FILES_LINK_MINUTES", false),
    ("NUAGE_KEEP_AWAKE_URL", false),
    ("NUAGE_KEEP_AWAKE_MINUTES", false),
    ("NUAGE_KEEP_AWAKE_HOURS", false),
    ("NUAGE_ENV_FILE", false),
    ("NUAGE_SETTINGS", false),
];

/// Reads the configuration through `var` (`std::env::var` in the binary).
#[cfg(feature = "server")]
pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Config> {
    let get = |k: &str| var(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let bad = |k: &str, v: &str| Error::Validation(format!("{k}=`{v}` is not valid"));
    let port = match get("PORT") {
        Some(p) => p.parse().map_err(|_| bad("PORT", &p))?,
        None => 8080,
    };
    let (data_dir, engine) = engine_from_vars(&var)?;
    let pg_port = match get("NUAGE_PG_PORT") {
        Some(p) => Some(p.parse().map_err(|_| bad("NUAGE_PG_PORT", &p))?),
        None => None,
    };
    let kv_port = match get("NUAGE_KV_PORT") {
        Some(p) => Some(p.parse().map_err(|_| bad("NUAGE_KV_PORT", &p))?),
        None => None,
    };
    let kv_bytes = crate::memory::kv_bytes(get("NUAGE_KV_MB").as_deref()).map_err(Error::Validation)?;
    let tls = match (get("NUAGE_TLS_CERT"), get("NUAGE_TLS_KEY")) {
        (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
        (None, None) => None,
        _ => return Err(Error::Validation("set both NUAGE_TLS_CERT and NUAGE_TLS_KEY, or neither".into())),
    };
    let admin_token = get("NUAGE_ADMIN_TOKEN");
    if let Some(t) = &admin_token
        && t.chars().count() < crate::server::MIN_ADMIN_TOKEN
    {
        return Err(Error::Validation(format!(
            "NUAGE_ADMIN_TOKEN is too short ({} characters): it makes applications, so it needs {} at least; `nuage genkey` makes one",
            t.chars().count(),
            crate::server::MIN_ADMIN_TOKEN
        )));
    }
    let files = match get("NUAGE_FILES_S3_BUCKET") {
        Some(bucket) => Some(Arc::new(file_store(&get, bucket)?)),
        None => None,
    };
    let pg_max_connections = match get("NUAGE_PG_MAX_CONNECTIONS") {
        Some(n) => n.parse::<usize>().map_err(|_| bad("NUAGE_PG_MAX_CONNECTIONS", &n))?.max(1),
        // Each has a thread: well under what the system lets this user start.
        None => crate::memory::thread_limit().map_or(100, |limit| (limit / 2).clamp(8, 100)),
    };
    let settings_read_only = match get("NUAGE_SETTINGS").as_deref() {
        None | Some("saved") => false,
        Some("environment") => true,
        Some(other) => {
            return Err(Error::Validation(format!(
                "NUAGE_SETTINGS=`{other}` is not valid: `environment` (the settings are those of the environment only), \
                 or nothing (what the console saved wins)"
            )));
        }
    };
    // What the environment says for the settings the owner may change later.
    let settings_env = crate::server::settings::DEFS
        .iter()
        .filter_map(|d| get(d.var).map(|v| (d.var.to_owned(), v)))
        .collect::<std::collections::HashMap<String, String>>();
    let startup = STARTUP
        .iter()
        .map(|&(var, secret)| crate::server::StartupSetting {
            var,
            value: get(var).map(|v| if secret { crate::server::StartupSetting::SECRET.to_owned() } else { v }),
        })
        .collect();
    let server = ServerOptions {
        settings_env: Some(settings_env),
        settings_read_only,
        startup,
        kv_bytes,
        kv_budget: crate::memory::kv_budget(get("NUAGE_KV_MB").as_deref()),
        files,
        admin_token,
        cache_budget: Some(engine.cache_bytes),
        cache_ceiling: Some(crate::memory::cache_ceiling(get("NUAGE_CACHE_MB").as_deref(), engine.cache_bytes)),
        pg_max_connections,
        pg_port,
        pg_tls: tls.is_some(),
        token: get("NUAGE_TOKEN"),
        admin_password: get("NUAGE_ADMIN_PASSWORD"),
        public_status: get("NUAGE_PUBLIC_STATUS").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes")),
        ..ServerOptions::default()
    };
    // A safety setting: a value mistyped must not mean `off`.
    let restore_required = match get("NUAGE_RESTORE_REQUIRED") {
        None => false,
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => true,
            "false" | "0" | "no" => false,
            _ => return Err(Error::Validation(format!("NUAGE_RESTORE_REQUIRED=`{v}`: `true` or `false`"))),
        },
    };
    let keep_awake = match get("NUAGE_KEEP_AWAKE_URL") {
        Some(url) => Some(crate::keepawake::KeepAwake::from_vars(
            &url,
            get("NUAGE_KEEP_AWAKE_MINUTES").as_deref(),
            get("NUAGE_KEEP_AWAKE_HOURS").as_deref(),
        )?),
        None => None,
    };
    // The same choice as `backup_store`: the folder, when the first has both.
    let backups = [FIRST, SECOND]
        .into_iter()
        .filter(|base| get(&format!("{base}_DIR")).or_else(|| get(&format!("{base}_S3_BUCKET"))).is_some())
        .map(|base| (base, get(&format!("{base}_DIR")).map(PathBuf::from)))
        .collect();
    Ok(Config {
        backup_dir: get("NUAGE_BACKUP_DIR").map(PathBuf::from),
        backups,
        restore_required,
        keep_awake,
        port,
        bind: get("NUAGE_BIND").unwrap_or_else(|| "0.0.0.0".into()),
        pg_bind: get("NUAGE_PG_BIND").or_else(|| get("NUAGE_BIND")).unwrap_or_else(|| "0.0.0.0".into()),
        kv_port,
        kv_bind: get("NUAGE_KV_BIND").or_else(|| get("NUAGE_BIND")).unwrap_or_else(|| "0.0.0.0".into()),
        pg_port,
        tls,
        data_dir,
        engine,
        server,
    })
}

/// [`from_vars`] with the process environment.
#[cfg(feature = "server")]
pub fn from_env() -> Result<Config> {
    from_vars(|k| std::env::var(k).ok())
}

/// [`from_vars`] with the process environment, then the settings file
/// `path`: a variable of the environment wins over the file. What
/// `nuage --env-file <file>` and `NUAGE_ENV_FILE` read: the services that the
/// installers set up on macOS and Windows start the server so.
#[cfg(feature = "server")]
pub fn from_env_file(path: &std::path::Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Validation(format!("cannot read the settings file {}: {e}", path.display())))?;
    let file: std::collections::HashMap<String, String> =
        parse_env_file(&text).map_err(|e| Error::Validation(format!("{}: {e}", path.display())))?.into_iter().collect();
    let mut config = from_vars(|k| std::env::var(k).ok().or_else(|| file.get(k).cloned()))?;
    config.server.settings_file = Some(path.display().to_string());
    Ok(config)
}

/// The variables of a settings file: `KEY=value` lines, `#` comments and blank
/// lines, an optional `export ` before the key, and the value as it is or
/// between `"` or `'` (what systemd's `EnvironmentFile`, Docker's `--env-file`
/// and a shell's `.` read alike). Later lines win.
pub fn parse_env_file(text: &str) -> std::result::Result<Vec<(String, String)>, String> {
    let mut vars = Vec::new();
    // A byte order mark, as Windows editors and PowerShell 5 write one.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: `KEY=value` expected", i + 1));
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("line {}: `{key}` is not a variable name", i + 1));
        }
        let value = value.trim();
        let value = [('"', '"'), ('\'', '\'')]
            .iter()
            .find_map(|&(a, b)| value.strip_prefix(a)?.strip_suffix(b))
            .filter(|_| value.len() >= 2)
            .unwrap_or(value);
        vars.push((key.to_owned(), value.to_owned()));
    }
    Ok(vars)
}

/// Are `a` and `b` on the same disk? Each is taken at its closest existing
/// folder. `None` when it cannot tell. A backup on the disk of the data dies
/// with it, and an external disk not plugged in leaves its mount point on the
/// computer's own disk: the server says so at startup.
pub fn same_disk(a: &std::path::Path, b: &std::path::Path) -> Option<bool> {
    fn existing(p: &std::path::Path) -> Option<PathBuf> {
        let p = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().ok()?.join(p) };
        p.ancestors().find(|a| a.exists()).map(std::path::Path::to_path_buf)
    }
    let (a, b) = (existing(a)?, existing(b)?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(std::fs::metadata(a).ok()?.dev() == std::fs::metadata(b).ok()?.dev())
    }
    #[cfg(windows)]
    {
        let root = |p: PathBuf| -> Option<String> {
            match std::fs::canonicalize(p).ok()?.components().next()? {
                std::path::Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().to_uppercase()),
                _ => None,
            }
        };
        Some(root(a)? == root(b)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (a, b);
        None
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn variables() {
        let vars = |k: &str| match k {
            "PORT" => Some("5000".into()),
            "NUAGE_TOKEN" => Some("t".into()),
            "NUAGE_PUBLIC_STATUS" => Some("true".into()),
            "NUAGE_BACKUP_DIR" => Some("/tmp/b".into()),
            "NUAGE_CACHE_MB" => Some("8".into()),
            _ => None,
        };
        let c = from_vars(vars).unwrap();
        assert_eq!((c.port, c.engine.cache_bytes, c.server.public_status), (5000, 8 << 20, true));
        assert_eq!(c.data_dir, PathBuf::from("nuage-data"));
        assert_eq!(c.bind, "0.0.0.0");
        assert_eq!(c.engine.backup.unwrap().store.describe(), "/tmp/b");
        assert!(from_vars(|k| (k == "PORT").then(|| "x".to_owned())).is_err());
        assert!(c.engine.encryption.is_none() && c.keep_awake.is_none());
        let awake = from_vars(|k| match k {
            "NUAGE_KEEP_AWAKE_URL" => Some("https://n.herokuapp.com/api/health".into()),
            "NUAGE_KEEP_AWAKE_HOURS" => Some("6-22".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(awake.keep_awake.unwrap().hours, Some((6, 22)));
        let key = from_vars(|k| (k == "NUAGE_ENCRYPTION_KEY").then(|| "0f".repeat(32))).unwrap();
        assert_eq!(key.engine.encryption.unwrap().kind(), "key");
        assert!(from_vars(|k| (k == "NUAGE_ENCRYPTION_KEY").then(|| "short".to_owned())).is_err());
        assert!(from_vars(|k| (k == "NUAGE_BACKUP_S3_BUCKET").then(|| "b".to_owned())).is_err(), "credentials needed");
        assert!(from_vars(|k| (k == "NUAGE_FILES_S3_BUCKET").then(|| "b".to_owned())).is_err(), "credentials needed");
        let r2 = |k: &str| {
            match k {
                "NUAGE_FILES_S3_BUCKET" => Some("tuiles"),
                "NUAGE_FILES_S3_ENDPOINT" => Some("https://abc.r2.cloudflarestorage.com"),
                "NUAGE_FILES_S3_ACCESS_KEY_ID" => Some("id"),
                "NUAGE_FILES_S3_SECRET_ACCESS_KEY" => Some("secret"),
                "NUAGE_FILES_LINK_MINUTES" => Some("120"),
                _ => None,
            }
            .map(str::to_owned)
        };
        let files = format!("{:?}", from_vars(r2).unwrap().server.files.unwrap());
        assert!(files.contains("region: \"auto\"") && files.contains("prefix: \"files/\""), "{files}");
        assert!(files.contains("link_seconds: 7200") && !files.contains("secret"), "{files}");
        assert!(from_vars(|k| if k == "NUAGE_FILES_LINK_MINUTES" { Some("0".into()) } else { r2(k) }).is_err());
    }

    #[test]
    fn settings_files() {
        let vars = parse_env_file(
            "# Nuage DB\n\nPORT=9090\nexport NUAGE_NAME = maison \nNUAGE_ADMIN_PASSWORD=\"deux mots\"\n\
             NUAGE_BACKUP_DIR='/Volumes/Sauvegarde/nuage'\nNUAGE_TOKEN=a=b\nPORT=9091\nEMPTY=\nQUOTE=\"\n",
        )
        .unwrap();
        let get = |k: &str| vars.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PORT"), Some("9091"), "the last line wins");
        assert_eq!(get("NUAGE_NAME"), Some("maison"));
        assert_eq!(get("NUAGE_ADMIN_PASSWORD"), Some("deux mots"));
        assert_eq!(get("NUAGE_BACKUP_DIR"), Some("/Volumes/Sauvegarde/nuage"));
        assert_eq!(get("NUAGE_TOKEN"), Some("a=b"));
        assert_eq!((get("EMPTY"), get("QUOTE")), (Some(""), Some("\"")));
        assert!(parse_env_file("PORT 9090").unwrap_err().contains("line 1"));
        let windows =
            parse_env_file("\u{feff}# Nuage DB\r\nPORT=9090\r\nNUAGE_BACKUP_DIR=E:\\Sauvegarde\\nuage\r\n").unwrap();
        assert_eq!(
            windows,
            [("PORT".into(), "9090".into()), ("NUAGE_BACKUP_DIR".into(), "E:\\Sauvegarde\\nuage".into())]
        );
        assert!(parse_env_file("ok=1\nBAD-KEY=1").unwrap_err().contains("line 2"));

        let dir = std::env::temp_dir().join(format!("nuage-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("nuage.env");
        std::fs::write(&file, "PORT=7070\nNUAGE_BACKUP_DIR=/tmp/sauvegarde\nNUAGE_PUBLIC_STATUS=true\n").unwrap();
        let c = from_env_file(&file).unwrap();
        assert_eq!((c.port, c.server.public_status), (7070, true));
        assert_eq!(c.backup_dir, Some(PathBuf::from("/tmp/sauvegarde")));
        assert!(from_env_file(&dir.join("absent.env")).unwrap_err().to_string().contains("cannot read"));
        std::fs::write(&file, "PORT=x").unwrap();
        assert!(from_env_file(&file).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn same_disk_or_not() {
        let tmp = std::env::temp_dir();
        assert_eq!(same_disk(&tmp, &tmp.join("not/yet/made")), Some(true), "a folder to come is where its parent is");
        #[cfg(target_os = "linux")]
        assert_eq!(same_disk(&tmp, std::path::Path::new("/proc")), Some(false), "/proc is a disk of its own");
    }

    /// A safety setting is read strictly: a value mistyped stops the start
    /// instead of meaning `false`.
    #[test]
    fn restore_required_is_true_or_false() {
        let with = |v: &str| {
            let v = v.to_owned();
            from_vars(move |k| (k == "NUAGE_RESTORE_REQUIRED").then(|| v.clone()))
        };
        assert!(!from_vars(|_| None).unwrap().restore_required, "off by default");
        for off in ["false", "0", "no", "", " "] {
            assert!(!with(off).unwrap().restore_required, "{off}");
        }
        for on in ["true", "1", "yes", " TRUE ", "Yes"] {
            assert!(with(on).unwrap().restore_required, "{on}");
        }
        for bad in ["ture", "on", "2", "y"] {
            let e = with(bad).unwrap_err().to_string();
            assert!(e.contains(&format!("NUAGE_RESTORE_REQUIRED=`{bad}`: `true` or `false`")), "{e}");
        }
    }

    /// Where the key-value stores live: in memory unless the setting says
    /// `database`; a value mistyped stops the start.
    #[test]
    fn where_the_key_value_store_lives() {
        let with = |v: &str| {
            let v = v.to_owned();
            from_vars(move |k| (k == "NUAGE_KV_STORAGE").then(|| v.clone()))
        };
        assert!(!from_vars(|_| None).unwrap().engine.kv_in_database, "in memory by default");
        for memory in ["memory", "", " Memory "] {
            assert!(!with(memory).unwrap().engine.kv_in_database, "{memory}");
        }
        for database in ["database", "DATABASE "] {
            assert!(with(database).unwrap().engine.kv_in_database, "{database}");
        }
        assert!(
            engine_from_vars(|k| (k == "NUAGE_KV_STORAGE").then(|| "database".to_owned())).unwrap().1.kv_in_database
        );
        for bad in ["disk", "db", "files"] {
            let e = with(bad).unwrap_err().to_string();
            assert!(e.contains(&format!("NUAGE_KV_STORAGE=`{bad}`: `memory` or `database`")), "{e}");
        }
    }

    /// A second backup destination: kept in step with the first, the only
    /// one when set alone, and refused when it is unclear or the same place.
    #[test]
    fn a_second_backup_destination() {
        let with = |vars: &[(&str, &str)]| {
            let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            from_vars(move |k| vars.get(k).cloned())
        };
        let describe = |c: &Config| c.engine.backup.as_ref().map(|b| b.store.describe());
        let s3 = [
            ("NUAGE_BACKUP2_S3_BUCKET", "other"),
            ("NUAGE_BACKUP2_S3_REGION", "eu-west-3"),
            ("NUAGE_BACKUP2_S3_PREFIX", "copy/"),
            ("NUAGE_BACKUP2_S3_ACCESS_KEY_ID", "id2"),
            ("NUAGE_BACKUP2_S3_SECRET_ACCESS_KEY", "secret2"),
        ];
        let both = with(&[[("NUAGE_BACKUP_DIR", "/mnt/backup")].as_slice(), &s3].concat()).unwrap();
        assert_eq!(describe(&both).as_deref(), Some("/mnt/backup and s3://other/copy/"));
        let (first, second) = both.engine.backup.as_ref().unwrap().store.parts().expect("two places");
        assert_eq!((first.describe(), second.describe()), ("/mnt/backup".into(), "s3://other/copy/".into()));
        assert_eq!(both.backups, [("NUAGE_BACKUP", Some(PathBuf::from("/mnt/backup"))), ("NUAGE_BACKUP2", None)]);
        let shown = format!("{:?}", both.engine.backup);
        assert!(shown.contains("s3://other/copy/") && !shown.contains("secret2"), "{shown}");

        // Its own credentials, else AWS_*: never the first's.
        let no_key = with(&[
            ("NUAGE_BACKUP_S3_BUCKET", "first"),
            ("NUAGE_BACKUP_S3_ACCESS_KEY_ID", "id"),
            ("NUAGE_BACKUP_S3_SECRET_ACCESS_KEY", "secret"),
            ("NUAGE_BACKUP2_S3_BUCKET", "other"),
        ]);
        let e = no_key.unwrap_err().to_string();
        assert!(e.contains("NUAGE_BACKUP2_S3_ACCESS_KEY_ID is missing"), "{e}");
        let aws = with(&[
            ("NUAGE_BACKUP2_S3_BUCKET", "other"),
            ("AWS_ACCESS_KEY_ID", "id"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]);
        assert_eq!(describe(&aws.unwrap()).as_deref(), Some("s3://other/nuage/"));

        // Alone, it is the only destination.
        let alone = with(&[("NUAGE_BACKUP2_DIR", "/mnt/new")]).unwrap();
        assert_eq!(describe(&alone).as_deref(), Some("/mnt/new"));
        assert!(alone.engine.backup.as_ref().unwrap().store.parts().is_none());
        assert_eq!(alone.backups, [("NUAGE_BACKUP2", Some(PathBuf::from("/mnt/new")))]);
        assert!(with(&[]).unwrap().backups.is_empty());

        // The first keeps its rule (the folder wins); the second says what it means.
        let first = with(&[("NUAGE_BACKUP_DIR", "/mnt/a"), ("NUAGE_BACKUP_S3_BUCKET", "b")]).unwrap();
        assert_eq!(describe(&first).as_deref(), Some("/mnt/a"));
        assert_eq!(first.backups, [("NUAGE_BACKUP", Some(PathBuf::from("/mnt/a")))]);
        let e = with(&[[("NUAGE_BACKUP2_DIR", "/mnt/b")].as_slice(), &s3].concat()).unwrap_err().to_string();
        assert!(e.contains("set NUAGE_BACKUP2_DIR or NUAGE_BACKUP2_S3_BUCKET, not both"), "{e}");
        // Its bucket mistyped or left out: never ignored, whichever of its settings is there.
        for (k, v) in &s3[1..] {
            let typo = with(&[("NUAGE_BACKUP_DIR", "/mnt/backup"), ("NUAGE_BACKUP2_S3_BUKET", "other"), (k, v)]);
            let e = typo.unwrap_err().to_string();
            let said = format!("{k} is set but NUAGE_BACKUP2_S3_BUCKET is not: set it (or NUAGE_BACKUP2_DIR)");
            assert!(e.contains(&said), "{e}");
        }
        let e = with(&[("NUAGE_BACKUP2_S3_ENDPOINT", "https://r2.example")]).unwrap_err().to_string();
        assert!(e.contains("NUAGE_BACKUP2_S3_ENDPOINT is set but NUAGE_BACKUP2_S3_BUCKET is not"), "{e}");
        // The first keeps its rule: its other settings alone are not looked at, as before.
        assert!(with(&[("NUAGE_BACKUP_S3_REGION", "eu-west-3")]).unwrap().backups.is_empty());

        // The same place twice: one copy, though two were asked for.
        let e = with(&[("NUAGE_BACKUP_DIR", "/mnt/backup"), ("NUAGE_BACKUP2_DIR", "/mnt//backup/")])
            .unwrap_err()
            .to_string();
        assert!(e.contains("the two backup destinations are the same place (/mnt/backup)"), "{e}");
        let tmp = std::env::temp_dir().join(format!("nuage-config-same-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("real")).unwrap();
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(tmp.join("link"));
            std::os::unix::fs::symlink(tmp.join("real"), tmp.join("link")).unwrap();
            let (real, link) = (tmp.join("real").display().to_string(), tmp.join("link").display().to_string());
            let e = with(&[("NUAGE_BACKUP_DIR", &real), ("NUAGE_BACKUP2_DIR", &link)]).unwrap_err().to_string();
            assert!(e.contains("are the same place"), "a link to the same folder: {e}");
        }
        std::fs::remove_dir_all(&tmp).unwrap();
        let bucket = |prefix: &str| {
            let prefix = prefix.to_owned();
            with(&[
                ("NUAGE_BACKUP_S3_BUCKET", "b"),
                ("NUAGE_BACKUP2_S3_BUCKET", "b"),
                ("NUAGE_BACKUP2_S3_PREFIX", &prefix),
                ("AWS_ACCESS_KEY_ID", "id"),
                ("AWS_SECRET_ACCESS_KEY", "secret"),
            ])
        };
        let e = bucket("nuage/").unwrap_err().to_string();
        assert!(e.contains("the two backup destinations are the same place (s3://b/nuage/)"), "{e}");
        assert_eq!(describe(&bucket("copy/").unwrap()).as_deref(), Some("s3://b/nuage/ and s3://b/copy/"));
        assert!(bucket("nuage").is_ok(), "`nuage` and `nuage/` are two places");
    }

    #[test]
    fn the_management_token_is_long_enough() {
        assert!(from_vars(|_| None).unwrap().server.admin_token.is_none());
        let long = "f".repeat(64);
        let on = from_vars(|k| (k == "NUAGE_ADMIN_TOKEN").then(|| format!("  {long} "))).unwrap();
        assert_eq!(on.server.admin_token.as_deref(), Some(long.as_str()));
        let short = from_vars(|k| (k == "NUAGE_ADMIN_TOKEN").then(|| "secret".to_owned())).unwrap_err();
        assert!(short.to_string().contains("too short") && short.to_string().contains("nuage genkey"), "{short}");
    }
}
