<div align="center">

[![Nuage DB](crates/nuage-site/static/og-image.png)](https://github.com/maxlestage/Nuage)

# Nuage DB

**Memoire's own database, in pure Rust. Embedded in your application or as a server, with the same code.**

[![CI](https://github.com/maxlestage/Nuage/actions/workflows/ci.yml/badge.svg)](https://github.com/maxlestage/Nuage/actions/workflows/ci.yml)
[![License: proprietary](https://img.shields.io/badge/license-proprietary-2a2a2e.svg)](#license)
[![Deploy to Heroku](https://www.herokucdn.com/deploy/button.svg)](https://heroku.com/deploy?template=https://github.com/maxlestage/Nuage)

**English** · [Français](README.fr.md) · [Español](README.es.md)

</div>

---

Nuage DB keeps your tables on disk behind a write-ahead log, makes instant copy-on-write
branches of a database, backs itself up to a second location and restores itself from there
at boot, and serves everything over HTTP(S) with a management console and a live dashboard.
It is the database of [Memoire](https://github.com/maxlestage/Memoire), the Rust ORM: the
same models and queries run on it embedded during development and tests, and on a Nuage DB
server in production, without changing a line.

## Features

| | |
|---|---|
| **Pages on disk** | Tables and indexes in one B-tree file with a bounded page cache (`NUAGE_CACHE_MB`): the data is not limited by memory. Primary-key and index lookups read only the rows they need. |
| **Write-ahead log** | Each commit is appended to the log and synced to disk before it is applied. Checkpoints, crash recovery, CRC-32 on log records and checksums on pages. |
| **Memoire semantics** | Statements run on Memoire's own evaluator over the rows read from disk: Nuage DB behaves exactly like Memoire's in-memory engine, on data of any size. |
| **Transactions** | Snapshot reads, optimistic commits (`Error::Conflict`, retried by `db.transaction(...)`), interactive transactions over HTTP, all-or-nothing batches. |
| **Branches** | A full copy of the database for a preview, a test or an experiment, made in an instant: it shares its parent's pages and stores only its own changes. |
| **Backups, restored at boot** | Snapshots plus log segments, to a folder or to S3-compatible storage. An empty data directory is restored automatically at startup, which makes hosts with an ephemeral disk work. |
| **Server** | One binary, configured by environment variables: the database protocol (`/v1`), the live status (`/api/status`, `/api/status/stream`), the console (`/admin`) and the website with its live dashboard (`/app`), on one port. |
| **WebAssembly client** | `memoire_orm::NuageDb` speaks HTTP(S): natively with Tokio, and in the browser through `fetch`. |
| **Upkeep on its own** | A background thread runs the checkpoints, ships the backups and compacts the file. |

## Two modes, one API

Nuage DB is used through [Memoire](https://github.com/maxlestage/Memoire)'s API: models,
the query builder, relations, transactions and migrations.

```toml
[dependencies]
# The engine, embedded mode and the server. `default-features = false` leaves the website out.
nuage = { git = "https://github.com/maxlestage/Nuage", default-features = false }
# The same Memoire as Nuage DB (the `nuage` feature is the client of a server).
memoire-orm = { git = "https://github.com/maxlestage/Memoire", rev = "fd1fdd5464547ce1f60f4a11e552c394da73cfc8", features = ["nuage"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### Embedded: a directory on disk

```rust
use memoire_orm::prelude::*;
use nuage::Embedded;

#[derive(Model, Debug, Clone)]
#[memoire(table = "notes")]
pub struct Note {
    #[memoire(primary_key, auto_increment)]
    pub id: i64,
    pub text: String,
}

#[tokio::main]
async fn main() -> memoire_orm::Result<()> {
    let db = Embedded::open("data")?; // created if missing
    db.create_table_if_not_exists::<Note>().await?;
    Note { id: 0, text: "hello".into() }.insert(&db).await?;
    println!("{:?}", Note::query().all(&db).await?);
    Ok(())
}
```

### Server: the same code over HTTP(S)

```rust
let db = NuageDb::connect("https://my-nuage.herokuapp.com", token).await?;
Note::query().filter(Note::TEXT.contains("hello")).all(&db).await?;
```

### One setting for both

```rust
// NUAGE_URL=data                                    -> embedded, in ./data
// NUAGE_URL=https://db.example.com NUAGE_TOKEN=...  -> a server
let db = nuage::Db::from_env().await?;
```

### Branches

```rust
db.create_branch("preview-42", None).await?;         // instant, whatever the size
let preview = db.on_branch("preview-42");
Note { id: 0, text: "only in the preview".into() }.insert(&preview).await?;
db.reset_branch("preview-42").await?;                // forget its changes
db.delete_branch("preview-42").await?;
```

## Quick start

```bash
git clone https://github.com/maxlestage/Nuage && cd Nuage
NUAGE_TOKEN=dev-token NUAGE_ADMIN_PASSWORD=dev-password cargo run -p nuage --release
# nuage: data is stored in /home/me/Nuage/nuage-data on this server's disk
# nuage: listening on 0.0.0.0:8080
```

Then open:

- http://localhost:8080/ : the website (English, French, Spanish);
- http://localhost:8080/app : the live dashboard (sign in with `NUAGE_ADMIN_PASSWORD`);
- http://localhost:8080/admin : the management console.

Applications connect with `NuageDb::connect("http://localhost:8080", "dev-token")`.

Docker:

```bash
docker build -t nuage .
docker run -p 8080:8080 -v nuage-data:/data -e NUAGE_TOKEN=... -e NUAGE_ADMIN_PASSWORD=... nuage
```

## Deploy on Heroku

The root of this repository deploys as is: `Procfile`, `RustConfig` and `heroku-build.sh`
(the `emk/rust` buildpack builds the `nuage` binary with the website), and `app.json` for the
**Deploy to Heroku** button above.

- **One Basic dyno.** A Basic dyno never sleeps. Run exactly one web dyno: two dynos would be
  two separate databases writing to the same backups.
- **The disk is ephemeral.** A dyno's disk is erased at every restart (at least once a day,
  and at each deploy). The data directory is only a working copy: **configure backups**, and
  each boot restores the database from them. On Heroku, use S3-compatible storage
  (`NUAGE_BACKUP_S3_*`); on a host with a persistent disk, a folder is enough
  (`NUAGE_BACKUP_DIR`).
- **Loss window.** Commits since the last shipped log segment (30 seconds by default) are
  lost only if the dyno stops without the `SIGTERM` Heroku sends first: on `SIGTERM` the
  server checkpoints and ships the last segment.

By hand:

```bash
heroku create my-nuage --buildpack emk/rust
heroku config:set NUAGE_TOKEN=$(openssl rand -hex 32) \
  NUAGE_ADMIN_PASSWORD='a long passphrase' \
  NUAGE_BACKUP_S3_BUCKET=my-bucket NUAGE_BACKUP_S3_REGION=eu-west-3 \
  NUAGE_BACKUP_S3_ACCESS_KEY_ID=... NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=...
git push heroku master
```

For storage other than AWS, set `NUAGE_BACKUP_S3_ENDPOINT` too. Then open
`https://my-nuage.herokuapp.com/admin`.

## Environment variables

| Variable | Meaning | Default |
|---|---|---|
| `PORT` | port to listen on (set by Heroku and most hosts) | `8080` |
| `NUAGE_DATA_DIR` | data directory | `nuage-data` |
| `NUAGE_NAME` | name of the database | `nuage` |
| `NUAGE_TOKEN` | token of the database API and of the status API | none: the API is off |
| `NUAGE_ADMIN_PASSWORD` | password of the console (and of the dashboard) | none: the console is off |
| `NUAGE_PUBLIC_STATUS` | `true`: `/api/status` and `/app` show a public subset without signing in | `false` |
| `NUAGE_CACHE_MB` | page cache, in MiB | `64` |
| `NUAGE_BACKUP_DIR` | back up to this folder | |
| `NUAGE_BACKUP_S3_BUCKET` | back up to this bucket | |
| `NUAGE_BACKUP_S3_ENDPOINT` | endpoint of S3-compatible storage | `https://s3.<region>.amazonaws.com` |
| `NUAGE_BACKUP_S3_REGION` | region | `us-east-1` |
| `NUAGE_BACKUP_S3_PREFIX` | prefix of the backup objects | `nuage/` |
| `NUAGE_BACKUP_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP_S3_SECRET_ACCESS_KEY` | credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_BACKUP_SNAPSHOT_MINUTES` | minutes between two snapshots | `60` |
| `SITE_URL` | public address of the website (canonical links, share card) | each request's host |

A commented example: [`nuage.env.example`](nuage.env.example). Serve the server over HTTPS
(most hosts do it for you): the token travels in a header, and the console's cookie is
`Secure`.

## `/app` and `/admin`

- **`/app`**, the live dashboard: requests per second, p50/p99 latency, connections,
  services, storage, cache, write-ahead log, backups, databases and branches, and a live event
  log. It is a Rust/WebAssembly island that reads `/api/status`, then follows
  `/api/status/stream` (Server-Sent Events, every second). It is private: the **Sign in**
  link goes through `/admin?next=/en/app/` and comes back to the dashboard. With
  `NUAGE_PUBLIC_STATUS=true`, visitors see the public subset. It installs as an app (PWA) and
  shows the last known state offline.
- **`/admin`**, the console: overview, tables and their rows, a query console (a small subset
  of SQL, with a confirmation before changes), branches, storage and buttons for a
  checkpoint, a backup, an integrity check and a compaction. Rendered by the server, it works
  without JavaScript, on a phone too. `HttpOnly`, `Secure`, `SameSite=Strict` session cookie,
  CSRF tokens, limited login attempts.

| Path | What | Access |
|---|---|---|
| `/v1/*` | the database protocol | `Authorization: Bearer $NUAGE_TOKEN` |
| `/api/health` | `{"status":"ok"}` | public |
| `/api/status`, `/api/status/stream` | live status, JSON and Server-Sent Events | token or console session |
| `/admin` | the console | `NUAGE_ADMIN_PASSWORD` |
| everything else | the website, `/app` included | public |

## Repository layout

```
.
├── Cargo.toml            workspace (memoire-orm pinned here)
├── crates/
│   ├── nuage/            the engine, embedded mode, server, console, status API,
│   │                     backups (folder, S3), the `nuage` binary; tests/common is
│   │                     Memoire's backend test suite
│   └── nuage-site/       the website: pages, the live /app dashboard (Rust/wasm, the
│                         prebuilt bundle in pkg/, rebuilt by build-wasm.sh), PWA, share card
├── Procfile, RustConfig, heroku-build.sh, app.json    Heroku
├── Dockerfile            any container host
├── nuage.env.example     the environment, commented
└── .github/workflows/ci.yml
```

The `site` feature of `crates/nuage` (on by default) mounts the website in the server
(`nuage::site()`); `default-features = false` leaves it out.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p nuage-site --lib --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace --all-features
crates/nuage-site/build-wasm.sh   # after changing the dashboard (Rust 1.99, wasm-bindgen of Cargo.lock)
```

## The family

- [Memoire](https://github.com/maxlestage/Memoire) ([website](https://memoire-7f4525f1fdaf.herokuapp.com/)): the Rust ORM whose database Nuage DB is.
- [Paquets](https://github.com/maxlestage/Paquets) ([website](https://paquets-90dfc06b6fea.herokuapp.com/)): the simplest full-stack framework for Rust.

## License

Proprietary: see [LICENSE](LICENSE). Copyright (c) 2026 Maxime Nathan Lestage.
