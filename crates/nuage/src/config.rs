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
//! | `NUAGE_KV_MB` | memory of each application's key-value store (`/v1/kv`, the key-value port), in MiB, or `auto`; past it, the least recently used keys go | `auto`: a sixteenth of the memory (4 to 256 MiB) each, and an eighth of it for all of them together |
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
//! | `NUAGE_BACKUP_DIR` | back up to this directory (another disk: a backup on the data's disk dies with it) | |
//! | `NUAGE_BACKUP_S3_BUCKET` | back up to this S3 bucket | |
//! | `NUAGE_BACKUP_S3_ENDPOINT` | S3 endpoint URL | `https://s3.<region>.amazonaws.com` |
//! | `NUAGE_BACKUP_S3_REGION` | S3 region | `us-east-1` |
//! | `NUAGE_BACKUP_S3_PREFIX` | prefix of the backup objects | `nuage/` |
//! | `NUAGE_BACKUP_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP_S3_SECRET_ACCESS_KEY` | S3 credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
//! | `NUAGE_BACKUP_SNAPSHOT_MINUTES` | minutes between two snapshots | `60` |
//! | `NUAGE_RESTORE_REQUIRED` | `true`: an empty data directory must be restored from the backup; when the backup holds nothing to restore, or no backup is set, the server refuses to start, and creates and writes nothing. For a disk erased at each restart (Heroku), once the server holds data: a first start has nothing to restore. `1`, `yes`, `0`, `no` too; any other value stops the start | `false`: an empty start is allowed, and said |
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
use crate::engine::{BackupOptions, DirectoryStore, EngineOptions};
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
    /// An empty data directory must be restored from the backup
    /// (`NUAGE_RESTORE_REQUIRED`): the binary opens it with
    /// [`Engine::open_existing`](crate::Engine::open_existing).
    pub restore_required: bool,
}

/// The data directory and the engine settings, through `var`: `NUAGE_DATA_DIR`,
/// `NUAGE_NAME`, `NUAGE_ENCRYPTION_KEY`, `NUAGE_CACHE_MB`, and the backups
/// (`NUAGE_BACKUP_DIR`, or `NUAGE_BACKUP_S3_*` with the feature `s3`,
/// `NUAGE_BACKUP_SNAPSHOT_MINUTES`). What an embedded database needs, without the server.
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
    let store: Option<Arc<dyn crate::engine::BackupStore>> = if let Some(dir) = get("NUAGE_BACKUP_DIR") {
        Some(Arc::new(DirectoryStore::new(dir)))
    } else if let Some(bucket) = get("NUAGE_BACKUP_S3_BUCKET") {
        s3_store(&get, bucket)?
    } else {
        None
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

#[cfg(feature = "s3")]
fn s3_store(
    get: &impl Fn(&str) -> Option<String>,
    bucket: String,
) -> Result<Option<Arc<dyn crate::engine::BackupStore>>> {
    let region = get("NUAGE_BACKUP_S3_REGION").unwrap_or_else(|| "us-east-1".into());
    let key = get("NUAGE_BACKUP_S3_ACCESS_KEY_ID")
        .or_else(|| get("AWS_ACCESS_KEY_ID"))
        .ok_or_else(|| Error::Validation("NUAGE_BACKUP_S3_ACCESS_KEY_ID is missing".into()))?;
    let secret = get("NUAGE_BACKUP_S3_SECRET_ACCESS_KEY")
        .or_else(|| get("AWS_SECRET_ACCESS_KEY"))
        .ok_or_else(|| Error::Validation("NUAGE_BACKUP_S3_SECRET_ACCESS_KEY is missing".into()))?;
    Ok(Some(Arc::new(S3Store::new(S3Options {
        endpoint: get("NUAGE_BACKUP_S3_ENDPOINT").unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com")),
        bucket,
        region,
        prefix: get("NUAGE_BACKUP_S3_PREFIX").unwrap_or_else(|| "nuage/".into()),
        access_key_id: key,
        secret_access_key: secret,
    })?)))
}

/// Without the feature `s3`, S3 backups are refused rather than silently ignored.
#[cfg(not(feature = "s3"))]
fn s3_store(
    _get: &impl Fn(&str) -> Option<String>,
    _bucket: String,
) -> Result<Option<Arc<dyn crate::engine::BackupStore>>> {
    Err(Error::Validation("NUAGE_BACKUP_S3_BUCKET is set, but this build of Nuage DB has no `s3` feature".into()))
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
    let server = ServerOptions {
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
    Ok(Config {
        backup_dir: get("NUAGE_BACKUP_DIR").map(PathBuf::from),
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
    from_vars(|k| std::env::var(k).ok().or_else(|| file.get(k).cloned()))
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
