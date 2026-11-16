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
| **Encrypted at rest** | Everything stored is encrypted (`NUAGE_ENCRYPTION_KEY`): the pages with XTS-AES-256, each log record with AES-256-GCM, and the backups, which are copies of them. Keys derived per application (HKDF, or Argon2id from a passphrase). |
| **Several applications** | One server, one database per application: its own tokens, its own keys, its own backups. The token an application sends picks its database. The console and `/app` show them side by side. |
| **Pages on disk** | Tables and indexes in one file of B-tree pages written by Nuage DB itself (copy-on-write, a checksum on each node), with a bounded page cache (`NUAGE_CACHE_MB`): the data is not limited by memory. Primary-key and index lookups read only the rows they need. |
| **Write-ahead log** | Each commit is appended to the log and synced to disk before it returns or is shown to a reader; commits made at the same time share one sync. Checkpoints, crash recovery, CRC-32 on log records and checksums on pages. |
| **Memoire semantics** | Statements run on Memoire's own evaluator over the rows read from disk: Nuage DB behaves exactly like Memoire's in-memory engine, on data of any size. |
| **Transactions** | Snapshot reads, optimistic commits (`Error::Conflict`, retried by `db.transaction(...)`), interactive transactions over HTTP, all-or-nothing batches. |
| **Branches** | A full copy of the database for a preview, a test or an experiment, made in an instant: it shares its parent's pages and stores only its own changes. |
| **Backups, restored at boot** | Snapshots plus log segments, to a folder or to S3-compatible storage, in one place or two kept in step. An empty data directory is restored automatically at startup, which makes hosts with an ephemeral disk work. A database never writes over another database's backup. |
| **Key-value store** | Each application has a store, in the server's memory or in its own database (`NUAGE_KV_STORAGE`): values with an expiry, counters, locks, a cache, over HTTP (`/v1/kv`) and on a port of its own ([Key-value store](#key-value-store)). |
| **Files** | Each application keeps files of any size (map tiles, photos, archives) in an S3-compatible bucket such as Cloudflare R2: the server lists them and signs the links, the bytes go straight between the client and the bucket ([Files](#files)). |
| **Server** | One binary, configured by environment variables: the database protocol (`/v1`), the live status (`/api/status`, `/api/status/stream`), the console (`/admin`) and the website with its live dashboard (`/app`), on one port. |
| **WebAssembly client** | `memoire_orm::NuageDb` speaks HTTP(S): natively with Tokio, and in the browser through `fetch`. |
| **Upkeep on its own** | A background thread runs the checkpoints, ships the backups and compacts the file. |

## Two modes, one API

Nuage DB is used through [Memoire](https://github.com/maxlestage/Memoire)'s API: models,
the query builder, relations, transactions and migrations.

```toml
[dependencies]
# The engine, embedded mode and the server, without the website.
nuage = { git = "https://github.com/maxlestage/Nuage", default-features = false, features = ["server"] }
# The same Memoire as Nuage DB (the `nuage` feature is the client of a server).
memoire-orm = { git = "https://github.com/maxlestage/Memoire", rev = "8f7e0cb", features = ["nuage"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

| Cargo feature | What it adds | Default |
|---|---|---|
| *(none)* | the embedded engine (`nuage::Embedded`), SQL, encryption, backups to a folder: **pure Rust**, no network stack, no C or assembly (RustCrypto) | always |
| `s3` | backups to S3-compatible storage (HTTP/1.1 over rustls) | yes |
| `server` | the server (`/v1`, PostgreSQL protocol, console, live status), `nuage::Db::Remote` and the `nuage` binary; implies `s3` | yes |
| `site` | the website and the `/app` dashboard, served by the binary; implies `server` | yes |
| `import-redb` | reads the page store of Nuage DB up to 0.4 (`nuage.redb`) once, to bring a data directory or a backup of those versions to this one (see [Coming from 0.4](#coming-from-04)); the only use of redb | yes |

`default-features = false` is the embedded engine alone; add `features = ["s3"]` for
S3 backups (the configuration of an embedded database with them:
`nuage::config::engine_from_vars`).

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

## Encryption

Nuage DB encrypts everything it stores (AES-256, HKDF-SHA256, Argon2id):

| What | How |
|---|---|
| pages (`nuage.db`: tables, indexes, branches, the list of applications) | XTS-AES-256 (IEEE 1619), the mode of disk encryption: pages keep their place and size |
| write-ahead log (`wal/*.log`) | AES-256-GCM per record, under a key of its own for each segment (random salt), the LSN as nonce |
| backups (folder or S3) | copies of the files above, encrypted the same way, with the directory's header |

`NUAGE_ENCRYPTION_KEY` is a key (`openssl rand -hex 32`) or a passphrase. Each data directory
has a header, `encryption.json`, with a random salt and a check value (never the key): its
root key is HKDF(key, salt), or Argon2id(passphrase, salt) (64 MiB, 3 passes), and the keys
of the pages, of the log and the check value derive from it. Each application has its own
directory, hence its own salt and keys. A wrong key is refused at startup, before anything is
read. A database or a backup stored in clear by an earlier version is encrypted when it is
opened with a key, and the backup in clear is replaced. **Keep the key**: without it, neither
the data nor the backups can be read. The embedded mode encrypts too, with
`EngineOptions { encryption: Some(Encryption::parse(key)?), .. }` or `NUAGE_ENCRYPTION_KEY`
with `nuage::Db::from_env()`.

### Coming from 0.4

Up to 0.4 the pages were a [redb](https://docs.rs/redb) file, `nuage.redb`. Since then they
are Nuage DB's own (`nuage.db`). Nothing to do: a data directory of an earlier version, or a
backup of one restored into an empty directory, is imported the first time this version opens
it, encrypted as it was, and the log written since its last checkpoint is replayed over it.
Before the new file takes its place it is read back and compared with the old one, entry by
entry: anything that differs stops the opening and leaves the old file as it was. The server
writes what each application holds (tables, rows) and what was imported in its startup log.
The old file stays beside the new one as `nuage.redb.imported`: delete it once you have
checked your data. When backups are configured, a snapshot of this version follows in a
moment, so the next restore no longer goes through the import; before it, the backup of the
earlier version (its manifest, snapshot, log segments and encryption header) is copied
under `before-import/` in the backup location, where nothing prunes it: the server's own
database at `before-import/`, each application at `before-import/apps/<name>/`, the shape
of the backup location itself. Going back is starting the earlier version on an empty data
directory with `before-import` as its backup location (`NUAGE_BACKUP_S3_PREFIX` or
`NUAGE_BACKUP_DIR` with `before-import/` added): it finds every application there. This
version restores from there too. Delete it once you have checked your data.

Reading the old file is the `import-redb` Cargo feature, on by default: the server has it.
An application that embeds the engine (`default-features = false`, with or without `s3`)
contains no redb at all and refuses such a directory with a message that says so; if it has
data or backups of those versions, it adds `features = ["import-redb"]`.
Once every database has been opened by this version, the feature can go.

## Several applications

One server holds several applications, each with a database of its own:

- the **first application** is the server's own database (`NUAGE_NAME`, data at the root of
  `NUAGE_DATA_DIR`, token `NUAGE_TOKEN`): an existing deployment keeps its data as is;
- the **others** are made in the console (`/admin`, **New application**), or with one
  request to the management API (`nuage apps create`, see below): a database in
  `apps/<name>/`, encrypted under keys of its own, backed up under `apps/<name>/`, and a first
  token, shown once (the server keeps only its SHA-256 digest);
- the **token** an application sends picks its database: `NuageDb::connect(url, token)` does
  not change, and an application never sees another's data, nor its transactions;
- their list lives in the first application's metadata: logged, encrypted, backed up and
  restored with it, then each application restores its own backup.

`Server::create_app`, `new_app_token`, `revoke_app_token`, `delete_app` and `apps` do the
same from Rust.

### Create an application in one command

The management API, `/api/apps`, makes applications and their tokens with one HTTP
request: from a script, a CI job, a deploy step, without the console's form. It opens with
a token of its own, `NUAGE_ADMIN_TOKEN`.

1. **Turn it on** once, on the server: `NUAGE_ADMIN_TOKEN=$(nuage genkey)` (32 characters at
   least; a shorter one stops the server at startup, with why). The installers set one
   already. On Heroku: the server's app → **Settings** → **Config Vars**. The startup log says
   `the management API (/api/apps) is on`.
2. **Create an application**. On the server's computer, nothing else is needed: `nuage`
   reads the token and the port in the installer's settings file (hence `sudo`; on Windows,
   a PowerShell as administrator), and shows `SITE_URL` as the address when the file has it:

   ```bash
   sudo nuage apps create shop "The shop"
   # NUAGE_URL=http://127.0.0.1:8080
   # NUAGE_TOKEN=nuage_3f9a12c0…
   ```

   From anywhere else, give it the server and its management token:

   ```bash
   export NUAGE_URL=https://my-nuage.herokuapp.com NUAGE_ADMIN_TOKEN=…
   nuage apps create shop "The shop"
   # NUAGE_URL=https://my-nuage.herokuapp.com
   # NUAGE_TOKEN=nuage_3f9a12c0…
   nuage apps list
   # nuage   1 token(s)
   # shop    1 token(s)   The shop
   ```

   or with any HTTP client:

   ```bash
   curl https://my-nuage.herokuapp.com/api/apps \
     -H "Authorization: Bearer $NUAGE_ADMIN_TOKEN" \
     -d '{"name": "shop", "description": "The shop"}'
   ```

   ```json
   {
     "application": { "name": "shop", "description": "The shop", "first": false, "created_at": 1794690000000,
                      "tokens": [{ "id": "3f9a12c0", "created_at": 1794690000000, "read_only": false }],
                      "env_token": false, "origins": [] },
     "token": "nuage_3f9a12c0…",
     "env": { "NUAGE_URL": "https://my-nuage.herokuapp.com", "NUAGE_TOKEN": "nuage_3f9a12c0…" },
     "message": "Application `shop` created, with its own encrypted database. Copy its token now: it is not shown again."
   }
   ```

3. **Connect the application**: set the two lines of `env` in its configuration (on Heroku,
   its Config Vars). It connects, and makes its tables at its first start.

From Rust, `memoire_orm::NuageAdmin` (Memoire's `nuage` feature) does the same:
`NuageAdmin::new(url, admin_token)?.create_app("shop", "The shop").await?` gives the token and
`env()`, the two lines. In a [Paquets](https://github.com/maxlestage/Paquets) project,
`paquets nuage create shop` makes the application and writes the two lines in `.env`.

| Method | Path | Body | Answer |
|---|---|---|---|
| `GET` | `/api/apps` | | `200 {"applications": [...]}`, the first one first, then by name |
| `POST` | `/api/apps` | `{"name": "shop", "description": "…"}` (description optional) | `201` the application, its `token` and `env`; `409` the name is taken; `400` an invalid name (1 to 63 of `a-z 0-9 - _`) |
| `GET` | `/api/apps/{name}` | | `200` the application; `404` |
| `POST` | `/api/apps/{name}/tokens` | `{"read_only": true}` (optional, `false` by default) | `201 {"application", "token", "read_only", "env"}` |
| `DELETE` | `/api/apps/{name}/tokens/{id}` | | `200 {"revoked": true}`; `404` no such token |

A refusal is JSON, `{"error": {"kind", "message"}}`, with a message that says what to do:
`401` without the management token or with a wrong one, `403` when `NUAGE_ADMIN_TOKEN` is
not set, `400`, `404`, `405`, `409`. The whole description is in
[`/v1/openapi.json`](crates/nuage/openapi.json).

What protects it:

- **A secret of its own**, not the console's password (a password is typed, so guessable) and
  not an application's token: a token opens its own database and nothing else, the first
  application's `NUAGE_TOKEN` included. It is compared in constant time.
- **A token is shown once**, in the answer that makes it: the server keeps only its SHA-256
  digest, and the list shows tokens by their id (their first characters).
- **No deletion**: deleting an application deletes its data and its backups, so it stays in
  the console, behind its name typed again.
- Keep `NUAGE_ADMIN_TOKEN` with the server's other secrets, never in an application or a
  page. To change it, set a new value: the old one stops working at the next start.

## SQL

Nuage DB speaks SQL (`POST /v1/sql`, the console's **Query** tab, `Embedded::sql` in Rust):

| | |
|---|---|
| **Reading** | `SELECT [DISTINCT]`, `JOIN`, `LEFT JOIN`, `CROSS JOIN` (`ON`, `USING`), `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY ... NULLS FIRST/LAST`, `LIMIT`/`OFFSET`, `UNION [ALL]`, `INTERSECT`, `EXCEPT`, subqueries (`IN`, `EXISTS`, scalar, correlated, in `FROM`) |
| **Expressions** | `CASE`, `CAST` and `::`, `\|\|`, `+ - * / %`, `LIKE`, `ILIKE`, `BETWEEN`, `IN`, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM` |
| **Functions** | `count`, `sum`, `avg`, `min`, `max`, `total`, `group_concat`/`string_agg`, `array_agg` (with `DISTINCT`); `lower`, `upper`, `length`, `trim`, `substr`, `replace`, `instr`, `lpad`, `concat`, `coalesce`, `nullif`, `iif`, `greatest`, `least`, `abs`, `round`, `floor`, `ceil`, `power`, `sqrt`, `random`, `now`, `current_date`, `date`, `extract`, `strftime`, `json_extract`, `json_object`, `json_array`, `gen_random_uuid`... |
| **Writing** | `INSERT ... VALUES / SELECT / DEFAULT VALUES`, `ON CONFLICT ... DO NOTHING / DO UPDATE SET ... excluded.x`, `INSERT OR IGNORE`, `REPLACE INTO`, `ON DUPLICATE KEY UPDATE`, `UPDATE ... SET col = <expression>`, `DELETE`, `RETURNING` |
| **Schema** | `CREATE TABLE [IF NOT EXISTS]` (types of PostgreSQL, MySQL and SQLite; `PRIMARY KEY`, `SERIAL`/`AUTOINCREMENT`, `NOT NULL`, `UNIQUE`, `DEFAULT`, `REFERENCES`, `UNIQUE (a, b)`), `CREATE TABLE ... AS SELECT`, `CREATE [UNIQUE] INDEX` (built from the rows there), `DROP INDEX`, `ALTER TABLE ... ADD COLUMN`, `DROP TABLE`, `EXPLAIN` |

A query Memoire's statements can express runs as one of them, through the indexes and stopping
at the end of the page; the rest (functions, `CASE`, subqueries, `UNION`) is computed by the
SQL layer on the rows the indexes found, with hash joins. Not there: `RIGHT`/`FULL` joins,
`WITH`, window functions, views, triggers, a primary key of several columns, changing a primary
key. A table created without a primary key gets a `rowid` column.

## Any language

Nuage DB speaks SQL over HTTP (`POST /v1/sql`, JSON in, JSON out): every language uses it,
with nothing to install. [`clients/`](clients) has a complete example, tested against a real
server in CI, for curl, JavaScript, TypeScript, Python, PHP, Go, Ruby, Java, Kotlin, C#, Swift, Dart
(Flutter), Elixir, Rust, C and Perl;
the console shows the same request in each language with the server's address. Every server
describes its API at `/v1/openapi.json` (OpenAPI 3.1) to generate a client for any other
language. Rust also has the native client, `NuageDb`. And
every PostgreSQL driver connects too: see below.

**Does my connection work?** `GET /v1/ping` answers which application the token opens:
`curl https://db.example.com/v1/ping -H "Authorization: Bearer <token>"` gives
`Connected to Nuage DB 0.4.6: application blog, read and write, branch main, 3 table(s).`,
and without a token, or with one that opens nothing, it says what is wrong. From any machine:
`nuage check <url> <token>`. A PostgreSQL connection gets the same line as a notice (psql
prints it), and each application's page in the console shows its last connection (when, how,
which client), or that it is waiting for a first one.

## PostgreSQL protocol

Nuage DB also speaks PostgreSQL's protocol (`NUAGE_PG_PORT=5432`; the installer and Docker
turn it on): **every language's PostgreSQL driver, ORM and tool connects as is**, with any
user name and **a token of an application as the password**; the database name picks a branch
(the application's name, or `main`: the main one).

```bash
PGPASSWORD=<token> psql "host=db.example.com port=5432 user=app dbname=nuage sslmode=require"
# postgresql://app:<token>@db.example.com:5432/nuage?sslmode=require
```

Tested in CI with psql, psycopg 2 and 3, SQLAlchemy, node-postgres, Postgres.js, pgx, Go's
database/sql, JDBC, PDO, the pg gem, tokio-postgres and Npgsql: examples in
[`clients/postgres/`](clients/postgres). Prepared statements, `$1` parameters (their types
come from the columns they meet), text and binary values, `BEGIN` ... `COMMIT` across
messages, savepoints, PostgreSQL's error codes (`40001` on a write conflict: run the
transaction again). `SELECT ... FOR UPDATE` (`SKIP LOCKED`, `NOWAIT`, `FOR SHARE`) and the advisory locks (`pg_advisory_xact_lock`, `hashtextextended`) are accepted: a transaction already fails at its commit (`40001`) if another one changed a table it read. `x IN (SELECT jsonb_array_elements_text($1::jsonb)::bigint)` takes a list as one JSON parameter. TLS with `NUAGE_TLS_CERT` and `NUAGE_TLS_KEY` (the installer uses the
domain's certificate). The SQL is Nuage DB's (above); the system catalogs (`pg_catalog`) are
not there, so tools that browse them (pgAdmin) show little, and Npgsql takes
`Server Compatibility Mode=NoTypeLoading`. Heroku routes only HTTP: there, use `/v1/sql`.

## Websites

Any website can use an application's database, from its server or from its pages:

- **`POST /v1/sql`**, from any language: full SQL with `?` (or `$1`) parameters, values
  never parsed as SQL (no injection). Rows come back as JSON objects (`"arrays": true` for
  arrays), an insert gives its `last_insert_id`. Several statements (separated by `;`, or
  `"statements": [...]`) run in one transaction, all or nothing.
- **From the browser**: list the website in the console (**Settings → Websites**:
  `https://site.fr`, `http://localhost:3000`, or `*`) and its pages may call `/v1` (CORS).
  The browser's preflight is cached for a day, so it costs one round trip, once.
- **Read-only tokens** (**Tokens → New read-only token**): a token in a page is public, so
  pages get one that reads and never writes; keep read-and-write tokens on servers.

```js
const res = await fetch("https://my-nuage.herokuapp.com/v1/sql", {
  method: "POST",
  headers: { "Authorization": "Bearer " + READ_ONLY_TOKEN, "Content-Type": "application/json" },
  body: JSON.stringify({ sql: "SELECT id, title FROM posts WHERE category = ? ORDER BY id DESC LIMIT 20", params: [3] }),
});
const { rows } = await res.json(); // [{ "id": 42, "title": "…" }, …]
```

```bash
curl https://my-nuage.herokuapp.com/v1/sql -H "Authorization: Bearer $NUAGE_TOKEN" \
  -d '{"sql": "INSERT INTO messages (name, text) VALUES (?, ?)", "params": ["Ada", "Hello"]}'
```

Speed, measured on the server's machine (`cargo run --release -p nuage --example bench`,
encrypted data, 20 000 rows, 4 cores): a row by key or by a unique column in **0.1 ms**,
a page of 20 rows through an index in **0.2 ms**, a durable update (fsync) in **0.5 ms**,
**85 000+ reads/s** with 32 clients (140 000+ on the key-value store). JSON answers of 1 KiB or more are gzip-compressed for clients that accept
it. What a visitor feels is mostly the network: pick the host's region closest to them, and a
dyno that never sleeps (Heroku **Basic**, not Eco, whose first request after a pause takes
seconds).

## Key-value store

Next to its database, each application has a **key-value store**: a cache in front of
anything slow, sessions, counters, rate limits, locks. By default it is in the server's
memory. Nothing touches the disk: a read or a write costs one round trip, and the server
answers more than 140 000 of them a second on 4 cores. A restart empties it. Memory is bounded
for each application (`NUAGE_KV_MB`): past the limit, the keys that expired go first, then the
least recently used ones.

**In memory or in the database.** `NUAGE_KV_STORAGE` says where the stores of all the
applications are kept. `memory`, the default, is the store above: the fastest, and emptied by
a restart. `database` keeps each application's keys in its own database, on disk. They are
written through the same log as its rows, so they survive a restart, are encrypted and are in
the backups. The requests, the commands and the port stay the same. With `database`:

- **a write waits for the disk**, like an update of a row: it is answered once it is synced
  (writes made at the same time share one sync). A read does not wait;
- **nothing is evicted**, and `NUAGE_KV_MB` is not used: the store is bounded by the disk, like
  the tables, so give cache entries an expiry. A value is 64 MiB at most;
- **a list of commands, or `MULTI` ... `EXEC`, is one commit**: all of it or nothing. A write
  the disk refuses is an error (`500` over HTTP, `-ERR storage: …` on the port), never half
  written;
- **an expiry is a date**: a key set to live an hour expires an hour later, whether the server
  restarted in between or not. An expired key is never returned or counted, and background
  passes remove it from the disk, one after the other while keys are due, as fast as keys
  expire;
- **`SCAN` answers page by page**: `COUNT` keys of the store at most per page (10 without it),
  and a cursor for the next page, `0` after the last, as Redis does: `scan_iter` and the like
  go through any number of keys, even when they delete them on the way. A cursor is kept 10
  minutes at least, whatever the other clients scan meanwhile; one given before a restart is
  refused (`ERR invalid cursor`: start again from `SCAN 0`), never taken for another page.
  `KEYS` answers at most 100,000 keys (16 MiB of them): past that, it says to use `SCAN`;
- **on a disk erased at each restart** (Heroku), a clean restart keeps every key: on `SIGTERM`
  the server stops taking writes, then ships its last log segment, so every write it answered
  is in it (one that comes after gets the storage error). After a crash, the keys come back as they were at the
  last segment shipped, like the rows (30 seconds by default): a lock released in that window is
  held again, a counter raised in it is back to its earlier value;
- the figures (`GET /v1/kv`, `/api/status`) say `"storage": "database"`, with `max_bytes` 0;
  `INFO` says `maxmemory:0` and `maxmemory_policy:noeviction`. Branches and SQL do not see the
  keys.

To change it, set the line in the settings file and restart the server (see the restart line
of [Install on any computer](#install-on-any-computer)); on Heroku, `heroku config:set
NUAGE_KV_STORAGE=database`, which restarts the dyno. The startup log says `key-value stores
are kept in each application's database`. Going from `memory` to `database` starts with what
the database already holds, nothing the first time: the keys in memory are not copied. Going
back to `memory` leaves the keys in the database, unread, and they are found again when
`database` comes back. Any other value stops the server at startup.

**Over HTTP**, from any language, with a token of the application (a read-only token reads):

| Request | What |
|---|---|
| `GET /v1/kv/<key>` | the value, as it was written (`404` when there is none); the header `nuage-ttl-ms` says how long it still lives |
| `PUT /v1/kv/<key>` (`?ttl=60`, `&nx`, `&xx`, `&keepttl`) | writes the body under the key: `{"set": true}`; `nx` writes only a key that does not exist (a lock) |
| `DELETE /v1/kv/<key>` | deletes it: `{"deleted": 1}` |
| `POST /v1/kv/<key>?incr` (`=5`, `&ttl=60`) | adds to a counter: `{"value": 6}`; `ttl` is the expiry of a counter this call creates (a rate limit in one request) |
| `POST /v1/kv/<key>?expire=60`, `?persist` | changes its expiry |
| `GET /v1/kv` (`?prefix=`, `&limit=`) | the keys (size, time left) and the figures of the store |
| `POST /v1/kv` | commands as JSON: `["SET", "k", "v", "EX", 60]` answers `{"result": "OK"}`; a list of commands answers `{"results": [...]}` in one round trip |
| `DELETE /v1/kv` | deletes every key |

```bash
curl -X PUT "https://my-nuage.herokuapp.com/v1/kv/session:42?ttl=1800" -H "Authorization: Bearer $NUAGE_TOKEN" -d 'ada'
curl "https://my-nuage.herokuapp.com/v1/kv/session:42" -H "Authorization: Bearer $NUAGE_TOKEN"        # ada
curl -X POST "https://my-nuage.herokuapp.com/v1/kv/hits?incr&ttl=60" -H "Authorization: Bearer $NUAGE_TOKEN"   # {"value":1}
curl https://my-nuage.herokuapp.com/v1/kv -H "Authorization: Bearer $NUAGE_TOKEN" -d '[["GET","session:42"],["TTL","session:42"]]'
```

**On a port of its own**: set `NUAGE_KV_PORT=6379` and the store also answers on a socket that
stays open, a token of the application as the password. Its wire is RESP (2 and 3), the one
Redis clients speak, so there is no client to write: `redis-cli`, redis-py, node-redis,
go-redis, Jedis... connect as they are:

```bash
redis-cli -u redis://default:<token>@db.example.com:6379 SET greeting hello EX 60
```

Strings and counters: `GET`, `SET` (`EX`, `PX`, `NX`, `XX`, `KEEPTTL`, `GET`), `SETNX`, `SETEX`,
`GETSET`, `GETDEL`, `GETEX`, `MGET`, `MSET`, `APPEND`, `STRLEN`, `INCR`, `DECR`, `INCRBY`,
`DECRBY`, `DEL`, `EXISTS`, `EXPIRE`, `PEXPIRE`, `TTL`, `PTTL`, `PERSIST`, `TYPE`, `KEYS`, `SCAN`,
`DBSIZE`, `FLUSHDB`, `INFO`, `PING`, and `MULTI` ... `EXEC` (the commands run as one). Lists,
hashes, sets and pub/sub are not there. With `NUAGE_TLS_CERT` and `NUAGE_TLS_KEY`, the same port
also answers TLS. Heroku routes only HTTP: there, use `/v1/kv`.

**From Rust**, Memoire's `NuageKv` (`db.kv()`): `get`, `set_for`, `incr_by`, `set_if_absent`,
and `remember(key, ttl, || compute())`, a cache in one call. Embedded, `nuage::kv::Kv` is the
store itself.

## Files

Next to its database, an application can keep **files** (map tiles, photos, archives of
several gigabytes) in an S3-compatible bucket: Cloudflare R2, AWS S3, MinIO, Scaleway...
The server keeps the list and signs the links; **the bytes go straight between the client and
the bucket**, never through the server: a small server (a Heroku dyno) hands out terabytes.
Each application has its own folder, `<prefix><application>/`: its token sees its files only.

| Request | What | Token |
|---|---|---|
| `GET /v1/files` (`?prefix=regions/`) | the files: `{"files": [{"name", "size", "updated", "etag"}], "count", "bytes"}` | read-only or read-write |
| `GET /v1/files/<name>` | `307` to a signed download link (`Range` works: a download resumes where it stopped); `?link`: the link as JSON, `{"url", "expires_at"}`; `?download=<file name>`: saved under that name | read-only or read-write |
| `POST /v1/files/<name>` | a signed upload link, `{"method": "PUT", "url", "expires_at"}`: `PUT` the bytes to it (5 GB at most each) | read-write |
| `DELETE /v1/files/<name>` | deletes the file | read-write |

A name is letters, digits, `.`, `_` and `-`, in parts separated by `/` (`regions/fr.tar`). A
link works an hour (`NUAGE_FILES_LINK_MINUTES`, 7 days at most); ask for a new one after.
The bucket refuses a link that comes with an `Authorization` header: curl and browsers drop it
when they follow the redirect; a client that keeps it (some HTTP libraries) takes `?link`, then
downloads the `url` without the token.

```bash
# Upload: a link, then the bytes to the bucket
url=$(curl -s -X POST https://my-nuage.herokuapp.com/v1/files/regions/fr.tar \
  -H "Authorization: Bearer $NUAGE_TOKEN" | jq -r .url)
curl -T fr.tar "$url"
# Download, resumable (-C -), with a read-only token
curl -L -C - -o fr.tar https://my-nuage.herokuapp.com/v1/files/regions/fr.tar -H "Authorization: Bearer $READ_TOKEN"
```

On Cloudflare R2 (no fee for the bytes downloaded): **R2 → Create bucket**, then **Manage API
tokens → Create API token**, *Object Read & Write* on that bucket. Set on the server:

```bash
heroku config:set NUAGE_FILES_S3_BUCKET=my-files \
  NUAGE_FILES_S3_ENDPOINT=https://<account id>.r2.cloudflarestorage.com \
  NUAGE_FILES_S3_ACCESS_KEY_ID=... NUAGE_FILES_S3_SECRET_ACCESS_KEY=...
```

## Install on any computer

Nuage DB runs on any computer: a Linux server, a Raspberry Pi, a Mac, a Windows PC, next to
(or instead of) a server on a host like Heroku. One command installs it as a service started
with the computer, and asks where the backups go.

| | One command | Service |
|---|---|---|
| **Linux** (Debian, Ubuntu, Fedora, Rocky, Alpine, Raspberry Pi OS..., x86_64 or ARM64, from 512 MiB) | `curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh \| sudo bash` | systemd `nuage` |
| **macOS** (Apple silicon or Intel), in Terminal | `curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh \| sudo bash` | launchd, as your user |
| **Windows** 10, 11, Server, in PowerShell **as administrator** | `irm https://raw.githubusercontent.com/maxlestage/Nuage/master/install.ps1 \| iex` | scheduled task `Nuage DB`, as SYSTEM |
| **cargo**, anywhere Rust runs | `cargo install --locked --git https://github.com/maxlestage/Nuage nuage` (see [With cargo](#with-cargo)) | none: hand it to the installer |
| **Docker**, anywhere | `docker compose up -d` in this repository (settings in `.env`) | the container |

Each one installs the `nuage` binary (a static build: no dependency), generates the settings
file with a key, a token, a console password and a management token, makes the data folder,
sets up the backups, starts the service and checks that it answers. It prints the address,
the token and the password once: **copy the settings file somewhere safe, away from this
computer**: nobody can read the data, nor its backups, without its key.

| | Linux | macOS | Windows |
|---|---|---|---|
| settings | `/etc/nuage/nuage.env` | `/usr/local/etc/nuage/nuage.env` | `C:\ProgramData\Nuage\nuage.env` |
| data | `/var/lib/nuage` | `/usr/local/var/nuage` | `C:\ProgramData\Nuage\data` |
| logs | `journalctl -u nuage -f` | `tail -f /usr/local/var/log/nuage.log` | `Get-Content -Wait C:\ProgramData\Nuage\nuage.log` |
| restart | `sudo systemctl restart nuage` | `sudo launchctl kickstart -k system/io.github.maxlestage.nuage` | `Stop-ScheduledTask -TaskName 'Nuage DB'; Get-Process nuage -ErrorAction SilentlyContinue \| Stop-Process -Force; Start-ScheduledTask -TaskName 'Nuage DB'` |

A change of the settings file is applied when the service restarts (with Docker:
`docker compose up -d` after a change of `.env`). On Windows the stop is forced: what was
written is replayed from the log at the next start.

On Linux it also **creates swap** when the server has none (the size of the memory, 1 to 4
GiB; `vm.swappiness=10`, so the memory goes to the database) and a `nuage` system user. On
macOS the service runs as the person who ran `sudo`; on Windows, as SYSTEM, readable by
administrators only, with a firewall rule for the local network.

- **HTTPS** (Linux): point a domain name to the server, then
  `curl -fsSL .../install.sh | sudo NUAGE_DOMAIN=db.example.com bash` (Caddy, with a free
  certificate; Nuage DB then listens on `127.0.0.1` only). Without HTTPS the console works,
  but warns that the password travels in clear.
- **Upgrade**: run the command again; data, key, settings and backups stay.
- **Options**: `NUAGE_PORT`, `NUAGE_PG_PORT` (`0`: off), `NUAGE_SWAP_MB` (`0`: no swap),
  `NUAGE_VERSION=v0.4.6`, `NUAGE_BINARY=/path/to/nuage`, `NUAGE_FROM_SOURCE=1` (build with
  cargo). On Windows, set them first: `$env:NUAGE_PORT = '9090'; irm ... | iex`.
- **Remove**: `... | sudo bash -s uninstall` keeps the data, the settings and the backups;
  `... | sudo bash -s purge` removes the data and the settings, never the backups. On Windows:
  `$env:NUAGE_ACTION = 'uninstall'` (or `'purge'`), then the same command.
- **Memory**: Nuage DB uses the memory of the server it is on, whatever its size, and no
  more. The data stays on disk; the memory goes to the page caches, which share one budget
  (`NUAGE_CACHE_MB=auto`) that follows the memory: a quarter of it at first, more while there
  is room and the caches are full (up to six tenths), less when memory gets tight. Each
  application has what it holds plus an equal part of what is free, so the budget goes to the
  applications that use it. Past 85 % in use, the caches are emptied, their budget halved and
  the freed memory returned to the system, while requests go on. The console's *Server* page
  and the dashboard's **Memory** card show the memory this server may use (its container's
  limit, where it has one: on a shared host, not the memory of the whole machine), what is
  used and what is left, in gigabytes.
- **By hand**: binaries for Linux, macOS and Windows are on the
  [releases page](https://github.com/maxlestage/Nuage/releases); `nuage --env-file nuage.env`
  starts the server with the settings of a file (`KEY=value` lines; the environment wins).

### With cargo

With Rust installed ([rustup.rs](https://rustup.rs)), on Linux, macOS or Windows:

```bash
cargo install --locked --git https://github.com/maxlestage/Nuage nuage
nuage --version
```

The `nuage` binary lands in `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows), with the
website and the live dashboard. A lighter server, without them (`/` is then a small home page;
the console, the API and everything else stay):
`cargo install --locked --git https://github.com/maxlestage/Nuage nuage --no-default-features --features server`.
A release: `--tag v0.4.6`. Upgrade: the same command, with `--force`.

`cargo install` only puts the binary there: no service, no settings, no backups. Hand it to
the installer, which does the rest (settings, backups, service started with the computer):

```bash
# Linux, macOS: `command -v nuage` finds it before sudo, whose PATH has no ~/.cargo/bin
curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh | sudo NUAGE_BINARY="$(command -v nuage)" bash
```

```powershell
# Windows, PowerShell as administrator
$env:NUAGE_BINARY = (Get-Command nuage).Source; irm https://raw.githubusercontent.com/maxlestage/Nuage/master/install.ps1 | iex
```

Or start it yourself, with a settings file: `nuage genkey` makes the key,
`nuage --env-file nuage.env` starts the server (see [`nuage.env.example`](nuage.env.example)).
On Linux and macOS, `NUAGE_FROM_SOURCE=1` makes the installer itself build with cargo.

### Backups: you choose where

The data folder is only one copy. With a backup, Nuage DB sends a **snapshot** of each
database (every hour, `NUAGE_BACKUP_SNAPSHOT_MINUTES`) and the **write-ahead log** since then
(at every checkpoint, every 30 seconds), all encrypted with the key; and when it starts with
an **empty data folder** (a new disk, a new computer), it **restores itself** from the backup
before it serves. The installer asks where they go; or say it in the command:

| Where | Command (Linux, macOS) | Windows |
|---|---|---|
| a folder on **another disk**: an external disk, a NAS | `curl ... \| sudo NUAGE_BACKUP_DIR=/Volumes/MyDisk/nuage bash` | `$env:NUAGE_BACKUP_DIR = 'E:\Nuage'; irm ... \| iex` |
| **S3-compatible storage**: AWS S3, Cloudflare R2, Scaleway, Backblaze B2, MinIO | `NUAGE_BACKUP_S3_BUCKET=… NUAGE_BACKUP_S3_REGION=… NUAGE_BACKUP_S3_ENDPOINT=… NUAGE_BACKUP_S3_ACCESS_KEY_ID=… NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=…` | the same, with `$env:` |
| none (not recommended) | `NUAGE_BACKUP=none` | `$env:NUAGE_BACKUP = 'none'` |

- **Another disk, or off-site.** A backup on the disk of the data dies with it: the installer
  and the server warn when the backup folder is on the same disk. A copy at home does not
  survive a fire or a theft: S3-compatible storage does (R2 and B2 cost little for this
  size). The settings file's key must be kept away from the backups' bucket too.
- **An external disk that is not plugged in**: the installer makes only the last folder of
  `NUAGE_BACKUP_DIR` (`/Volumes/MyDisk` must exist), and the server warns at startup when the
  folder turns out to be on the computer's own disk. On macOS, if the backups to an external
  disk fail (in the log), add `/usr/local/bin/nuage` in System Settings → Privacy & Security
  → Full Disk Access. On Windows, a NAS share (`\\nas\backup\nuage`) must let the computer's
  account (`COMPUTER$`) write.
- **Change** where they go, on a disk that is not erased (your computer, a server): run the
  installer again with the new option. It replaces the old one in the settings file and
  restarts the service; without an option, it keeps them. For a bucket, changing the settings
  file and restarting the service does the same. On Linux, a new folder goes through the
  installer, which also lets the service write there. Right after the start, the server sends
  a whole snapshot of every application to the new place. The old place is left as it is:
  delete it once the console shows the new one's backups.
- **A database never writes over another database's backup.** Before each snapshot, the
  server compares the destination's `encryption.json` with its own. A place that holds
  another database's backup is refused, and nothing is written there. The event log says
  `backup failed: … holds the backup of another database (its encryption.json is not this
  one's): nothing was written there`, and the console and `/app` show the backup failing.
  Give this database a place of its own, or empty that one. A bucket that does not exist is an
  error too, never an empty backup.
- **An empty start is said, and can be refused.** When the data folder is empty and the backup
  holds nothing to restore, the server starts a new, empty database and says so in its log
  (`warning: the data directory was empty and … holds no backup: this is a NEW, EMPTY
  database`) and in the event log; so does an application whose backup holds nothing. With
  `NUAGE_RESTORE_REQUIRED=true`, that start is refused instead: the server stops and says what
  to check, having created nothing and written nothing. A registered application whose backup
  holds nothing is not served either: the server starts without it, and the console lists it
  as one that cannot be opened (complete its backup and restart, or delete it there). Set it
  once the server holds data: a first start has nothing to restore.
- **Two places at once**: `NUAGE_BACKUP2_DIR`, or `NUAGE_BACKUP2_S3_BUCKET` and the other
  `NUAGE_BACKUP2_S3_*`, add a second destination kept in step with the first. Each one gets
  the snapshots and the log with its own progress: one that does not answer never holds the
  other back (each has its own thread), and gets a whole snapshot when it answers again. An
  empty data folder is restored from one of them, whole: the one that reaches furthest. The
  server does not start when either does not answer, since it may be the one that reaches
  furthest (take it out of the settings to start from the other alone; a second that has not
  answered 5 seconds after the first counts as not answering), or when the two hold different
  databases. Without the first, `NUAGE_BACKUP2_*` is the only destination: adding the second,
  waiting until it has everything, swapping the two, then removing the old one moves the
  backups without a stop (see Heroku below). The console's *Storage* page shows each
  destination's last backup, and `/api/status` each one in `backup.destinations`: a
  destination shows no time until it has received a whole snapshot since the start, and the
  last backup shows none until both have. A round to a destination that has not ended after
  10 minutes (one that does not answer, or so slow it never keeps up) shows as its error, and
  in the event log, until it ends. The stop sends to both at the same time: the first gets as
  long as it takes, the second 20 seconds at most.
- **Deleting an application** in the console empties its backup (`apps/<name>/`) in each
  destination, once the backup round running to it, if any, has ended: nothing of it is
  written there afterwards. One that cannot be emptied is named in the answer and the event
  log; until its backup there is deleted by hand, no application of that name can be created,
  so that a new one never starts with the deleted one's rows. One that has not answered
  within 10 seconds is named too, and its emptying goes on: the name stays taken until it
  ends. On Linux, the installer's service may write
  only to the data folder and `NUAGE_BACKUP_DIR`: there, make the second one a bucket.
- **Check**: the console's *Storage* page, and `/app`, show the last backup and its errors. A
  bucket that stops answering fails the round after 60 seconds with nothing moving, and the
  next round tries again; an upload is never cut while its bytes keep leaving the server (the
  limit counts from the last byte that went on the line, not from what is waiting to go).
- **Restore** on a new computer: install, put the old settings file's
  `NUAGE_ENCRYPTION_KEY` and the same backup settings in the new one, empty the data folder
  and restart the service: it restores the last snapshot and replays the log after it.
- **One writer per backup**: two servers must never back up to the same folder or bucket
  prefix. A server with a different database is refused there (see above), but two copies of
  one database, such as two dynos restored from the same backup, would overwrite each other.
  A computer next to a Heroku server uses a folder, bucket or `NUAGE_BACKUP_S3_PREFIX` of its
  own.

## Quick start

```bash
git clone https://github.com/maxlestage/Nuage && cd Nuage
# The key everything is encrypted with: keep it, the data cannot be read without it.
export NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32)
NUAGE_TOKEN=dev-token NUAGE_ADMIN_PASSWORD=dev-password cargo run -p nuage --release
# nuage: data is stored in /home/me/Nuage/nuage-data on this server's disk
# nuage: listening on 0.0.0.0:8080
```

Then open:

- http://localhost:8080/ : the website (English, French, Spanish);
- http://localhost:8080/app : the live dashboard (sign in with `NUAGE_ADMIN_PASSWORD`);
- http://localhost:8080/admin : the management console.

Applications connect with `NuageDb::connect("http://localhost:8080", "dev-token")`.

Docker (Compose, with the settings in `.env`, from [`nuage.env.example`](nuage.env.example)):

```bash
cp nuage.env.example .env   # then set NUAGE_ENCRYPTION_KEY, NUAGE_TOKEN, NUAGE_ADMIN_PASSWORD
docker compose up -d
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
  server stops taking writes (one that comes after gets an error, never an answer the backup
  lacks), checkpoints and ships the last segment. The keys of the key-value stores follow the
  same rule with `NUAGE_KV_STORAGE=database`; in memory, a restart empties them.
- **A change of settings restarts the dyno.** Each `heroku config:set` or `config:unset`
  restarts it: its disk is erased and the database is restored from the backup that the new
  settings name. So never point the backup variables, in one command, at a place that does
  not hold the backup: the dyno would find nothing there and start a new, empty database
  (its log says so, and the old place keeps its backups). Move them as below.
- **Refuse an empty start.** Once the server holds data, set
  `heroku config:set NUAGE_RESTORE_REQUIRED=true`. From then on, a dyno that finds nothing to
  restore (a mistyped bucket or prefix, the backup variables removed) does not start, and
  creates and writes nothing. Heroku shows it as crashed, `heroku logs --tail` says what to
  check, and putting the settings back starts it from its backup. Not on a first deployment,
  which has nothing to restore: `app.json` leaves it unset.
- **Move the backups without a stop.** To move them to another bucket:
  (1) add the new one as the second destination, in one command (`heroku config:set
  NUAGE_BACKUP2_S3_BUCKET=new-bucket NUAGE_BACKUP2_S3_REGION=… NUAGE_BACKUP2_S3_ACCESS_KEY_ID=…
  NUAGE_BACKUP2_S3_SECRET_ACCESS_KEY=…`, and `NUAGE_BACKUP2_S3_ENDPOINT=…` for storage other
  than AWS: the second never takes the first's endpoint): the dyno restarts from the old one
  and sends a whole snapshot of every application to the new one; (2) wait until the
  console's *Storage* page, or `/app`, shows the last backup of every application with no
  error (it shows none until the new bucket has a whole snapshot of each), or until
  `backup.destinations[1].last_at` of `/api/status` has a time; (3) swap them, in one command:
  the new bucket's settings as `NUAGE_BACKUP_S3_*` and the old one's as `NUAGE_BACKUP2_S3_*`
  (bucket, region, keys, prefix and endpoint of each): the stopping dyno sends its last
  seconds to both, and the next one restores from the one that reaches furthest; (4) remove
  the old one, now the second, in one command (`heroku config:unset NUAGE_BACKUP2_S3_BUCKET
  NUAGE_BACKUP2_S3_REGION …`): the stopping dyno sends its last seconds to the new bucket,
  now the first, as long as that takes (the second only gets 20 seconds), and the next one
  restores from it, the only destination.

By hand:

```bash
heroku create my-nuage --buildpack emk/rust
heroku config:set NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32) NUAGE_TOKEN=$(openssl rand -hex 32) \
  NUAGE_ADMIN_PASSWORD='a long passphrase' \
  NUAGE_BACKUP_S3_BUCKET=my-bucket NUAGE_BACKUP_S3_REGION=eu-west-3 \
  NUAGE_BACKUP_S3_ACCESS_KEY_ID=... NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=...
git push heroku master
```

For storage other than AWS, set `NUAGE_BACKUP_S3_ENDPOINT` too. Then open
`https://my-nuage.herokuapp.com/admin`: its *Storage* page (or `backup.last_ok` in
`/api/status`) should show the first backup with no error.

To move the backups **with a stop** instead, when the storage's own tools copy a large backup
faster than the dyno sends it:

```bash
heroku config:set NUAGE_RESTORE_REQUIRED=true        # once: from now on an empty start is refused
heroku ps:scale web=0                                # the server ships its last backup and stops
aws s3 sync s3://old-bucket/nuage/ s3://new-bucket/nuage/   # every object (or rclone sync)
heroku config:set NUAGE_BACKUP_S3_BUCKET=new-bucket NUAGE_BACKUP_S3_REGION=… \
  NUAGE_BACKUP_S3_ACCESS_KEY_ID=… NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=…   # all of them, in one command
heroku ps:scale web=1                                # it restores from the new bucket, or does not start
```

## Environment variables

| Variable | Meaning | Default |
|---|---|---|
| `PORT` | port to listen on (set by Heroku and most hosts) | `8080` |
| `NUAGE_PG_PORT` | also serve the PostgreSQL protocol on this port (`5432`): every language's PostgreSQL driver, a token as the password | off |
| `NUAGE_PG_BIND` | its address, when the website stays behind a proxy on `127.0.0.1` | `NUAGE_BIND` |
| `NUAGE_TLS_CERT`, `NUAGE_TLS_KEY` | PEM files of a certificate and its key: TLS for the PostgreSQL protocol | none |
| `NUAGE_BIND` | address to listen on (`127.0.0.1`: this machine only, behind a proxy) | `0.0.0.0` |
| `NUAGE_DATA_DIR` | data directory | `nuage-data` |
| `NUAGE_NAME` | name of the first application (and of its database) | `nuage` |
| `NUAGE_ENCRYPTION_KEY` | the key everything is encrypted with: 64 hex digits (`openssl rand -hex 32`, `nuage genkey`), base64 of 32 bytes, or a passphrase of 12 characters or more | **required**: the binary does not start without it |
| `NUAGE_TOKEN` | a token of the first application (named `NUAGE_NAME`); the console makes the other applications and their tokens | none |
| `NUAGE_ADMIN_PASSWORD` | password of the console (and of the dashboard) | none: the console is off |
| `NUAGE_ADMIN_TOKEN` | token of the management API (`/api/apps`, `nuage apps`): creates applications and their tokens; 32 characters at least (`nuage genkey`) | none: the API is off |
| `NUAGE_PUBLIC_STATUS` | `true`: `/api/status` and `/app` show a public subset without signing in | `false` |
| `NUAGE_CACHE_MB` | memory the page caches of all the applications may hold together, in MiB, or `auto` | `auto`: it follows the memory: a quarter of it at first, more while there is room (up to six tenths), less when it gets tight |
| `NUAGE_MEMORY_MB` | the memory to count on, in MiB, for a host that limits it without saying so | the container's limit, else the machine's memory |
| `NUAGE_PG_MAX_CONNECTIONS` | PostgreSQL connections served at once (each has a thread) | `100`, less where the system allows few threads |
| `NUAGE_ENV_FILE` | read the settings from this file too (`KEY=value` lines; `nuage --env-file <file>` does the same); the environment wins over the file | none |
| `NUAGE_BACKUP_DIR` | back up to this folder, on another disk (the server warns when it is on the data's disk) | |
| `NUAGE_BACKUP_S3_BUCKET` | back up to this bucket | |
| `NUAGE_BACKUP_S3_ENDPOINT` | endpoint of S3-compatible storage | `https://s3.<region>.amazonaws.com` |
| `NUAGE_BACKUP_S3_REGION` | region | `us-east-1` |
| `NUAGE_BACKUP_S3_PREFIX` | prefix of the backup objects | `nuage/` |
| `NUAGE_BACKUP_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP_S3_SECRET_ACCESS_KEY` | credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_BACKUP_SNAPSHOT_MINUTES` | minutes between two snapshots (of each destination) | `60` |
| `NUAGE_BACKUP2_DIR` | a second backup destination kept in step with the first: this folder (see the backups above); alone, it is the only destination | |
| `NUAGE_BACKUP2_S3_BUCKET` | the second destination in this bucket (with `NUAGE_BACKUP2_DIR` too, or the same place as the first, the server does not start) | |
| `NUAGE_BACKUP2_S3_ENDPOINT`, `NUAGE_BACKUP2_S3_REGION`, `NUAGE_BACKUP2_S3_PREFIX` | its endpoint, region and prefix, set for it: never the first's | `https://s3.<region>.amazonaws.com`, `us-east-1`, `nuage/` |
| `NUAGE_BACKUP2_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP2_S3_SECRET_ACCESS_KEY` | its credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_RESTORE_REQUIRED` | `true`: an empty data directory must be restored from the backup; with nothing to restore, or no backup set, the server does not start, and creates and writes nothing (nor does it serve a registered application whose backup holds nothing). For a disk erased at each restart, once the server holds data (see [Deploy on Heroku](#deploy-on-heroku)) | `false`: an empty start is allowed, and said in the log |
| `NUAGE_KV_PORT` | also serve the key-value store on a port of its own (`6379`), over the wire Redis clients speak: one connects to an application's store, a token as the password | off |
| `NUAGE_KV_BIND` | its address | `NUAGE_BIND` |
| `NUAGE_KV_MB` | memory of each application's key-value store, in MiB, or `auto`; past it, the least recently used keys go. Not used with `NUAGE_KV_STORAGE=database` | `auto`: a sixteenth of the memory, 4 to 256 MiB |
| `NUAGE_KV_STORAGE` | where the key-value stores of all the applications are kept: `memory` (the fastest; a restart empties them) or `database` (in each application's database: kept across restarts and in the backups; a write waits for the disk; see [Key-value store](#key-value-store)) | `memory` |
| `NUAGE_FILES_S3_BUCKET` | keep the applications' files (`/v1/files`) in this S3-compatible bucket (see [Files](#files)) | none: no files |
| `NUAGE_FILES_S3_ENDPOINT` | its endpoint (`https://<account id>.r2.cloudflarestorage.com` on R2) | `https://s3.<region>.amazonaws.com` |
| `NUAGE_FILES_S3_REGION` | its region | `auto` on R2, else `us-east-1` |
| `NUAGE_FILES_S3_PREFIX` | prefix of the objects: an application's files go under `<prefix><application>/` | `files/` |
| `NUAGE_FILES_S3_ACCESS_KEY_ID`, `NUAGE_FILES_S3_SECRET_ACCESS_KEY` | its credentials (else `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_FILES_LINK_MINUTES` | how long a download or upload link works (10080 at most) | `60` |
| `NUAGE_KEEP_AWAKE_URL` | the server requests this address (its own `https://…/api/health`) so a host that puts idle servers to sleep (Heroku Eco) keeps it awake; an awake server only: asleep, the next visitor wakes it | off |
| `NUAGE_KEEP_AWAKE_MINUTES` | minutes between two requests (under Eco's 30) | `20` |
| `NUAGE_KEEP_AWAKE_HOURS` | only in these hours, UTC (`6-22`); outside them it may sleep | always |
| `SITE_URL` | public address of the website (canonical links, share card) | each request's host |

A commented example: [`nuage.env.example`](nuage.env.example). Serve the server over HTTPS
(most hosts do it for you; on your own server, the installer's `NUAGE_DOMAIN`): the token
travels in a header, and the console's cookie is `Secure` over HTTPS.

## `/app` and `/admin`

- **`/app`**, the live dashboard: requests per second, p50/p99 latency, connections,
  services, storage, cache, write-ahead log, backups, one card per application (its requests, latency, data, branches, encryption; chips narrow the cards and the log to one), and a live event
  log. It is a Rust/WebAssembly island that reads `/api/status`, then follows
  `/api/status/stream` (Server-Sent Events, every second). It is private: the **Sign in**
  link goes through `/admin?next=/en/app/` and comes back to the dashboard. With
  `NUAGE_PUBLIC_STATUS=true`, visitors see the public subset. It installs as an app (PWA) and
  shows the last known state offline.
- **`/admin`**, the console (in English, French and Spanish: the browser's language, or the EN · FR · ES links of every page): the applications (cards; create one, its token is shown once), then for each one its overview, tables and their rows, its tokens (new, revoke), its settings (description, deletion confirmed by its name), a query console (all of SQL,
  with a confirmation before changes), branches, storage and buttons for a
  checkpoint, a backup, an integrity check and a compaction. Rendered by the server, it works
  without JavaScript, on a phone too. `HttpOnly`, `Secure`, `SameSite=Strict` session cookie,
  CSRF tokens, limited login attempts.

| Path | What | Access |
|---|---|---|
| `/v1/*` | the database protocol, on the database of the application the token opens | `Authorization: Bearer <token of an application>` |
| `POST /v1/sql` | SQL with `?` parameters, rows as JSON objects (websites, any language) | a token of the application; a read-only token reads only |
| `/v1/kv` | the application's key-value store (see [Key-value store](#key-value-store)) | a token of the application; a read-only token reads |
| `/v1/files` | the application's files, through signed links to its bucket (see [Files](#files)) | a token of the application; a read-only token lists and downloads |
| `/api/health` | `{"status":"ok"}` | public |
| `/api/status`, `/api/status/stream` | live status, JSON and Server-Sent Events | an application's token (that application) or a console session (all of them) |
| `/api/apps` | the management API: applications and their tokens (see [Create an application in one command](#create-an-application-in-one-command)) | `Authorization: Bearer <NUAGE_ADMIN_TOKEN>` |
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
