# Changelog

All notable changes to Nuage DB. Versions follow [Semantic Versioning](https://semver.org/).

## 0.3.0 (2026-11-13)

### Install on any server

- `install.sh`: `curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh | sudo bash`
  installs Nuage DB on a Linux server (x86_64 or ARM64): the static binary (checked with its
  sha256; else built from source), swap when the server has none (the size of the memory, 1
  to 4 GiB, `vm.swappiness=10`), a `nuage` system user, `/var/lib/nuage`,
  `/etc/nuage/nuage.env` with a generated key, token and password (kept on upgrades), a
  hardened systemd service, the firewall (ufw, firewalld), and with `NUAGE_DOMAIN` HTTPS
  through Caddy. `uninstall` and `purge` remove it.
- Releases: each `v*` tag publishes the binary for Linux (static, musl, x86_64 and ARM64),
  macOS (ARM64, x86_64) and Windows, with their sha256.
- `docker-compose.yml`.
- `NUAGE_BIND`: the address to listen on (`127.0.0.1` behind a proxy); `nuage --version`.
- The console's cookies are `Secure` over HTTPS and on `localhost`; a server reached in plain
  HTTP (just installed, by its IP) can log in, and its login page warns that the password
  travels in clear.

### Memory

- `NUAGE_CACHE_MB=auto` (the default): each application's page cache is an eighth of the
  memory, 8 to 512 MiB (it was 64 MiB), the container's limit included.
- The console's *Server* page and `/api/status` (`memory`) show the memory Nuage DB uses, the
  free memory, the swap and the cache of each application.

### Documentation

- Nuage DB is documented on its own: the references to another project are gone.


### Encryption at rest

- Everything Nuage DB stores is encrypted (AES-256, HKDF-SHA256, Argon2id): the page store with XTS-AES-256 (a
  storage backend of redb), each write-ahead log record with AES-256-GCM (a key per segment,
  from a random salt; the LSN as nonce), and the backups, copies of both.
- `NUAGE_ENCRYPTION_KEY`: a 32-byte key (64 hex digits or base64; `nuage genkey`) or a
  passphrase (Argon2id, 64 MiB, 3 passes). Each data directory has a header
  (`encryption.json`: salt, check value, never the key), copied to the backup; a wrong key is
  refused at startup.
- **The `nuage` binary requires `NUAGE_ENCRYPTION_KEY`.** A database or a backup stored in
  clear is encrypted when it is opened with a key, and the backup in clear is replaced.
- `EngineOptions::encryption` and `nuage::Encryption` for the embedded mode;
  `Db::from_env()` reads `NUAGE_ENCRYPTION_KEY`.

### Several applications

- One server, several applications, each with a database of its own (directory, keys,
  backups under `apps/<name>/`) and tokens of its own (only their SHA-256 digests are kept).
  The token sent on `/v1` picks the database; transactions belong to their application.
- The first application is the server's own database (`NUAGE_NAME`, `NUAGE_TOKEN`): existing
  deployments keep their data. The list of applications lives in its metadata (logged,
  encrypted, backed up, restored first).
- `Server::create_app`, `new_app_token`, `revoke_app_token`, `delete_app`, `apps`,
  `app_engine`; `nuage::PrefixedStore`.
- `/api/status`: every application for a console session, its own for an application's token,
  each with its requests, latency, tables, rows, size, branches and encryption; `encryption`
  and `applications` at the top; `log` events name their application.

### Console in three languages

- The console (`/admin`) in English, French and Spanish: the browser's language
  (`Accept-Language`), or the EN · FR · ES links of every page (a cookie); decimal commas in
  French and Spanish. A text without its translation stops debug builds and the tests.

### SQL

- Full SQL (`POST /v1/sql`, the console's Query tab, `Embedded::sql`): `SELECT` with joins
  (`JOIN`, `LEFT JOIN`, `CROSS JOIN`, `USING`), `GROUP BY`/`HAVING`, `UNION`/`INTERSECT`/`EXCEPT`,
  subqueries (`IN`, `EXISTS`, scalar, correlated, derived tables), `CASE`, `CAST`, `||`, `%`,
  `ILIKE`, `IS DISTINCT FROM`, aggregates and some sixty scalar functions; `INSERT` with
  `ON CONFLICT`, `OR IGNORE`, `REPLACE`, `RETURNING`, `INSERT ... SELECT`; `UPDATE` and `DELETE`
  with any expression and `RETURNING`; `CREATE TABLE` (PostgreSQL, MySQL and SQLite types,
  `DEFAULT`, `REFERENCES`, `UNIQUE (a, b)`), `CREATE TABLE AS`, `CREATE INDEX` on existing
  rows, `DROP INDEX`, `ALTER TABLE ADD COLUMN`, `EXPLAIN`; several statements in one
  transaction; `?`, `?N` and `$N` parameters.
- What Memoire's statements express runs as one of them (indexes, early stop); the rest is
  computed by the SQL layer on the rows the indexes found, with hash joins.
- The engine adds indexes to tables that have rows (and drops them), and columns.

### Websites and speed

- `POST /v1/sql`: SQL from any language or a page (`SELECT`, `INSERT`, `UPDATE`, `DELETE`,
  `CREATE TABLE`, `DROP TABLE`), `?` parameters, rows as JSON objects, `last_insert_id`.
- The websites of an application (console: **Settings → Websites**, `Server::set_app_origins`):
  their pages call `/v1` from the browser (CORS); the preflight is cached for a day.
- Read-only tokens for pages (`Server::new_app_read_token`): writes, transactions and branch
  changes are refused with `403`.
- JSON answers of 1 KiB or more are gzip-compressed when the client accepts it.
- `WHERE indexed = ? ORDER BY id [DESC] LIMIT n` (and `LIMIT n` alone) read only the rows they
  need: a page of 20 rows through an index goes from 1.3 ms to 0.5 ms.
- `examples/bench.rs`: latency and throughput of a server, from a client on the same machine.
- `NUAGE_KEEP_AWAKE_URL` (`_MINUTES`, `_HOURS`): the server requests its own address so a host
  that puts idle servers to sleep (Heroku Eco) keeps it awake, always or in given hours.

### Interface

- The console: an **Applications** page (one card each, a form to create one; its token is
  shown once with how to connect), and for each application tabs for its overview, tables,
  query console, branches, storage, tokens (new, revoke) and settings (description, deletion
  confirmed by its name); a **Server** page with the events of every application. The old
  addresses (`/admin/tables`...) lead to the first application.
- The dashboard `/app`: one card per application (requests per second with its own
  sparkline, p99, errors, tables, rows, size, last request, branches, encryption), chips to
  narrow the cards and the event log to one application, an "Encrypted" badge.
- The website: "Encrypted at rest" and "Several applications" among the features.

## 0.2.1 (2026-11-13)

First release of Nuage DB in its own repository (it was developed in
[Memoire](https://github.com/maxlestage/Memoire), whose version it keeps).

### Engine

- Tables and indexes on disk in one B-tree file, with a bounded page cache (`NUAGE_CACHE_MB`).
- Write-ahead log synced on each commit, checkpoints, crash recovery, CRC-32 on log records
  and checksums on pages.
- Statements run by Memoire's evaluator over the rows read from disk: primary-key and index
  lookups, streamed scans, early `LIMIT`, `COUNT(*)` from a counter.
- Snapshot transactions with optimistic commits (`Error::Conflict`), batches.
- Instant copy-on-write branches: create, reset, delete, branches of branches.
- Backups to a folder or to S3-compatible storage (snapshots plus log segments), restored
  automatically into an empty data directory at startup; last segment shipped on `SIGTERM`.
- Background upkeep: checkpoints, backups, compaction.

### Embedded and server

- `nuage::Embedded`: the engine in your process, with Memoire's `Executor` and `Database`
  traits; `nuage::Db::from_env()` picks embedded or remote from `NUAGE_URL`.
- The `nuage` binary and `nuage::Server`: the database protocol (`/v1`) for
  `memoire_orm::NuageDb`, `/api/health`, live status (`/api/status`, `/api/status/stream`),
  configured by environment variables.
- The console at `/admin`: overview, tables, query console, branches, storage and upkeep
  actions; works without JavaScript, CSRF-protected, limited login attempts,
  `/admin?next=...` returns to the page that asked.

### Website

- The website of Nuage DB (`crates/nuage-site`), served by the binary with the `site` feature
  (on by default): pages in English, French and Spanish, the live dashboard at `/app`
  (Rust/WebAssembly, fed by `/api/status/stream`), PWA with offline support, share card.

### Repository

- Cargo workspace (`crates/nuage`, `crates/nuage-site`); memoire-orm pinned to a Memoire
  commit until 0.2.1 is published on crates.io.
- Deploys to Heroku as is (`Procfile`, `RustConfig`, `heroku-build.sh`, `app.json` for the
  Deploy to Heroku button), and with the `Dockerfile` on any container host.
- CI: format, clippy (native and wasm32), tests, MSRV 1.95, and a check that the dashboard's
  browser bundle (`crates/nuage-site/pkg`) is up to date.
