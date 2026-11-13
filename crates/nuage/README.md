# Nuage DB

Memoire's own database, in pure Rust: tables on disk behind a write-ahead log,
copy-on-write branches, backups to a directory or S3-compatible storage, an HTTP
server with live status and a management console. Use it embedded in your
application or as a server, with the same [Memoire](https://crates.io/crates/memoire-orm)
code.

```rust,no_run
use memoire_orm::prelude::*;

#[derive(Model, Debug)]
struct Note { #[memoire(primary_key, auto_increment)] id: i64, text: String }

# async fn demo() -> memoire_orm::Result<()> {
// Embedded: a directory on disk, encrypted (pages, log, backups).
let options = nuage::EngineOptions {
    encryption: Some(nuage::Encryption::parse(&std::env::var("NUAGE_ENCRYPTION_KEY").unwrap_or_default())?),
    ..Default::default()
};
let db = nuage::Embedded::open_with("data", options)?;
db.create_table_if_not_exists::<Note>().await?;
Note { id: 0, text: "hello".into() }.insert(&db).await?;

// Or a server, picked from `NUAGE_URL` (a directory or an https:// URL) and `NUAGE_TOKEN`.
let db = nuage::Db::from_env().await?;
# Ok(()) }
```

Run the server:

```bash
NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32) NUAGE_TOKEN=... NUAGE_ADMIN_PASSWORD=... cargo run -p nuage --release
```

- **Encryption**: everything stored is encrypted (`NUAGE_ENCRYPTION_KEY`, a key or a
  passphrase): pages with XTS-AES-256, log records with AES-256-GCM, backups as copies of
  them; keys derived per data directory (HKDF, Argon2id for a passphrase).
- **Applications**: one server, one database per application, with its own tokens, keys and
  backups; the token an application sends picks its database.
- **Engine**: redb B-tree pages with a bounded cache, a write-ahead log synced on each
  commit, checkpoints, crash recovery, checksums; Memoire's evaluator over the rows read
  from disk.
- **Branches**: instant, they share their parent's pages and store only their own changes.
- **Backups**: snapshots plus log segments, restored automatically into an empty data
  directory (hosts with an ephemeral disk, such as Heroku).
- **Server**: `/v1` (the database protocol, for `memoire_orm::NuageDb`), `/api/status`
  and `/api/status/stream`, the console at `/admin`, and with the `site` feature (on by
  default) the website of Nuage DB with its live dashboard at `/app`.

Configuration, deployment (any host, Docker, Heroku) and limits: see the
[repository](https://github.com/maxlestage/Nuage) (its root deploys to Heroku as is) and the
[documentation](https://github.com/maxlestage/Memoire/blob/master/docs/en/nuage.md).
