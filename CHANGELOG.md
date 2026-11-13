# Changelog

All notable changes to Nuage DB. Versions follow [Semantic Versioning](https://semver.org/).

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
