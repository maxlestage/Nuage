//! Configuration of the `nuage` binary, from environment variables.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `PORT` | port to listen on (set by Heroku and most hosts) | `8080` |
//! | `NUAGE_DATA_DIR` | data directory | `nuage-data` |
//! | `NUAGE_NAME` | name of the database | `nuage` |
//! | `NUAGE_TOKEN` | token of the database API and of the status API | none: the API is off |
//! | `NUAGE_ADMIN_PASSWORD` | password of the console (`/admin`) | none: the console is off |
//! | `NUAGE_PUBLIC_STATUS` | `true`: `/api/status` shows a public subset without credentials | `false` |
//! | `NUAGE_CACHE_MB` | page cache, in MiB | `64` |
//! | `NUAGE_BACKUP_DIR` | back up to this directory | |
//! | `NUAGE_BACKUP_S3_BUCKET` | back up to this S3 bucket | |
//! | `NUAGE_BACKUP_S3_ENDPOINT` | S3 endpoint URL | `https://s3.<region>.amazonaws.com` |
//! | `NUAGE_BACKUP_S3_REGION` | S3 region | `us-east-1` |
//! | `NUAGE_BACKUP_S3_PREFIX` | prefix of the backup objects | `nuage/` |
//! | `NUAGE_BACKUP_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP_S3_SECRET_ACCESS_KEY` | S3 credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
//! | `NUAGE_BACKUP_SNAPSHOT_MINUTES` | minutes between two snapshots | `60` |

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use memoire_orm::{Error, Result};

use crate::engine::{BackupOptions, DirectoryStore, EngineOptions};
use crate::s3::{S3Options, S3Store};
use crate::server::ServerOptions;

/// Everything the binary needs.
#[derive(Debug)]
pub struct Config {
    /// Port to listen on.
    pub port: u16,
    /// Data directory.
    pub data_dir: PathBuf,
    /// Engine settings.
    pub engine: EngineOptions,
    /// Server settings.
    pub server: ServerOptions,
}

/// Reads the configuration through `var` (`std::env::var` in the binary).
pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Config> {
    let get = |k: &str| var(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let bad = |k: &str, v: &str| Error::Validation(format!("{k}=`{v}` is not valid"));
    let port = match get("PORT") {
        Some(p) => p.parse().map_err(|_| bad("PORT", &p))?,
        None => 8080,
    };
    let mut engine = EngineOptions::default();
    if let Some(name) = get("NUAGE_NAME") {
        engine.name = name;
    }
    if let Some(mb) = get("NUAGE_CACHE_MB") {
        engine.cache_bytes = mb.parse::<usize>().map_err(|_| bad("NUAGE_CACHE_MB", &mb))? << 20;
    }
    let store: Option<Arc<dyn crate::engine::BackupStore>> = if let Some(dir) = get("NUAGE_BACKUP_DIR") {
        Some(Arc::new(DirectoryStore::new(dir)))
    } else if let Some(bucket) = get("NUAGE_BACKUP_S3_BUCKET") {
        let region = get("NUAGE_BACKUP_S3_REGION").unwrap_or_else(|| "us-east-1".into());
        let key = get("NUAGE_BACKUP_S3_ACCESS_KEY_ID")
            .or_else(|| get("AWS_ACCESS_KEY_ID"))
            .ok_or_else(|| Error::Validation("NUAGE_BACKUP_S3_ACCESS_KEY_ID is missing".into()))?;
        let secret = get("NUAGE_BACKUP_S3_SECRET_ACCESS_KEY")
            .or_else(|| get("AWS_SECRET_ACCESS_KEY"))
            .ok_or_else(|| Error::Validation("NUAGE_BACKUP_S3_SECRET_ACCESS_KEY is missing".into()))?;
        Some(Arc::new(S3Store::new(S3Options {
            endpoint: get("NUAGE_BACKUP_S3_ENDPOINT").unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com")),
            bucket,
            region,
            prefix: get("NUAGE_BACKUP_S3_PREFIX").unwrap_or_else(|| "nuage/".into()),
            access_key_id: key,
            secret_access_key: secret,
        })?))
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
    let server = ServerOptions {
        token: get("NUAGE_TOKEN"),
        admin_password: get("NUAGE_ADMIN_PASSWORD"),
        public_status: get("NUAGE_PUBLIC_STATUS").is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes")),
        ..ServerOptions::default()
    };
    Ok(Config {
        port,
        data_dir: get("NUAGE_DATA_DIR").map_or_else(|| PathBuf::from("nuage-data"), PathBuf::from),
        engine,
        server,
    })
}

/// [`from_vars`] with the process environment.
pub fn from_env() -> Result<Config> {
    from_vars(|k| std::env::var(k).ok())
}

#[cfg(test)]
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
        assert_eq!(c.engine.backup.unwrap().store.describe(), "/tmp/b");
        assert!(from_vars(|k| (k == "PORT").then(|| "x".to_owned())).is_err());
        assert!(from_vars(|k| (k == "NUAGE_BACKUP_S3_BUCKET").then(|| "b".to_owned())).is_err(), "credentials needed");
    }
}
