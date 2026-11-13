//! # Nuage DB
//!
//! Memoire's own database, in pure Rust: tables on disk behind a write-ahead
//! log, copy-on-write branches, backups, an HTTP server with a management
//! console. Use it two ways, with the same Memoire code:
//!
//! * **Embedded**: [`Embedded::open("data")`](Embedded::open) runs the engine
//!   inside your application (development, tests, desktop and edge apps).
//! * **Server**: the `nuage` binary (or [`Server`]) serves a database over
//!   HTTP(S); applications reach it with `memoire_orm::NuageDb`, natively or
//!   from WebAssembly in the browser.
//!
//! [`Db::from_env`] picks one from configuration (`NUAGE_URL`), so the same
//! code uses an embedded database in development and a server in production.
//!
//! ```no_run
//! # async fn demo() -> memoire_orm::Result<()> {
//! use memoire_orm::prelude::*;
//!
//! #[derive(Model, Debug)]
//! struct Note { #[memoire(primary_key, auto_increment)] id: i64, text: String }
//!
//! // `NUAGE_URL=data` (a directory) or `NUAGE_URL=https://nuage.example.com`
//! // with `NUAGE_TOKEN`.
//! let db = nuage::Db::from_env().await?;
//! db.create_table_if_not_exists::<Note>().await?;
//! Note { id: 0, text: "hello".into() }.insert(&db).await?;
//! # Ok(()) }
//! ```
//!
//! ## How it works
//!
//! * **Pages**: [redb](https://docs.rs/redb), a pure-Rust B-tree store, keeps
//!   tables and indexes in one file with a bounded page cache: the data is not
//!   limited by memory, and primary-key and index lookups read only the rows
//!   they need.
//! * **Write-ahead log**: each commit is appended to the log and synced to
//!   disk before it is applied; checkpoints make the page store durable;
//!   startup replays what the last checkpoint missed. Log records and pages
//!   carry checksums.
//! * **Statements**: Memoire's own evaluator ([`memoire_orm::evaluator`]),
//!   over the rows read from disk: the semantics of `MemoryDb`, on data of
//!   any size.
//! * **Branches**: a branch shares every page of its parent and stores only
//!   its own changes, so making one is instant.
//! * **Upkeep**: a background thread checkpoints, ships backups (to a
//!   directory or S3-compatible storage) and compacts the file. An empty data
//!   directory is restored from the backup at startup, which makes hosts with
//!   ephemeral disks (Heroku) work.

#![warn(missing_docs, rust_2018_idioms)]

pub mod config;
pub mod crypto;
mod embedded;
pub mod engine;
pub mod s3;
pub mod server;
#[cfg(feature = "site")]
mod site;

use memoire_orm::{
    Database, Dialect, ExecResult, Executor, FromRow, NuageDb, NuageTransaction, Query, Result, Transaction,
};

pub use crypto::Encryption;
pub use embedded::{Embedded, EmbeddedTransaction, TableInfo};
pub use engine::{
    BackupOptions, BackupStatus, BackupStore, DirectoryStore, Engine, EngineOptions, Event, PrefixedStore, StorageStats,
};
pub use memoire_orm::nuage::Branch;
pub use server::{AppInfo, Server, ServerOptions, SiteHandler, SiteRequest, SiteResponse, TokenInfo};
#[cfg(feature = "site")]
pub use site::site;

/// The version of Nuage DB.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A Nuage DB database, embedded or remote: the same API either way.
#[derive(Clone, Debug)]
pub enum Db {
    /// The engine in this process.
    Embedded(Embedded),
    /// A Nuage DB server.
    Remote(NuageDb),
}

impl Db {
    /// Opens `target`: an `http://` or `https://` URL is a server (with
    /// `token`), anything else a data directory (in clear: see
    /// [`Db::from_env`] and [`EngineOptions::encryption`]).
    pub async fn open(target: &str, token: Option<&str>) -> Result<Db> {
        if target.starts_with("http://") || target.starts_with("https://") {
            let mut b = NuageDb::builder(target);
            if let Some(t) = token {
                b = b.token(t);
            }
            Ok(Db::Remote(b.connect().await?))
        } else {
            Ok(Db::Embedded(Embedded::open(target)?))
        }
    }

    /// [`Db::open`] with `NUAGE_URL` (a URL or a directory; default: the
    /// directory `nuage-data`) and `NUAGE_TOKEN`. An embedded database is
    /// encrypted with `NUAGE_ENCRYPTION_KEY` when it is set.
    pub async fn from_env() -> Result<Db> {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        let target = var("NUAGE_URL").unwrap_or_else(|| "nuage-data".into());
        let token = var("NUAGE_TOKEN");
        match var("NUAGE_ENCRYPTION_KEY") {
            Some(secret) if !target.starts_with("http://") && !target.starts_with("https://") => {
                let options =
                    EngineOptions { encryption: Some(Encryption::parse(&secret)?), ..EngineOptions::default() };
                Ok(Db::Embedded(Embedded::open_with(&target, options)?))
            }
            _ => Db::open(&target, token.as_deref()).await,
        }
    }
}

impl Executor for Db {
    fn dialect(&self) -> Option<Dialect> {
        None
    }

    async fn fetch_all<O>(&self, query: Query<'_>) -> Result<Vec<O>>
    where
        O: FromRow + Send,
    {
        match self {
            Db::Embedded(db) => db.fetch_all(query).await,
            Db::Remote(db) => db.fetch_all(query).await,
        }
    }

    async fn execute(&self, query: Query<'_>) -> Result<ExecResult> {
        match self {
            Db::Embedded(db) => db.execute(query).await,
            Db::Remote(db) => db.execute(query).await,
        }
    }
}

/// A transaction of a [`Db`].
#[derive(Debug)]
pub enum DbTransaction {
    /// Of an embedded database.
    Embedded(EmbeddedTransaction),
    /// On a server.
    Remote(NuageTransaction),
}

impl Database for Db {
    type Tx<'c> = DbTransaction;

    async fn begin(&self) -> Result<DbTransaction> {
        Ok(match self {
            Db::Embedded(db) => DbTransaction::Embedded(db.begin().await?),
            Db::Remote(db) => DbTransaction::Remote(db.begin().await?),
        })
    }
}

impl Executor for DbTransaction {
    fn dialect(&self) -> Option<Dialect> {
        None
    }

    async fn fetch_all<O>(&self, query: Query<'_>) -> Result<Vec<O>>
    where
        O: FromRow + Send,
    {
        match self {
            DbTransaction::Embedded(tx) => tx.fetch_all(query).await,
            DbTransaction::Remote(tx) => tx.fetch_all(query).await,
        }
    }

    async fn execute(&self, query: Query<'_>) -> Result<ExecResult> {
        match self {
            DbTransaction::Embedded(tx) => tx.execute(query).await,
            DbTransaction::Remote(tx) => tx.execute(query).await,
        }
    }
}

impl Transaction for DbTransaction {
    async fn commit(self) -> Result<()> {
        match self {
            DbTransaction::Embedded(tx) => tx.commit().await,
            DbTransaction::Remote(tx) => tx.commit().await,
        }
    }

    async fn rollback(self) -> Result<()> {
        match self {
            DbTransaction::Embedded(tx) => tx.rollback().await,
            DbTransaction::Remote(tx) => tx.rollback().await,
        }
    }
}

/// Compiles the code examples of the crate README.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
