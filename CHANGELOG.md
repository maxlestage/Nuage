# Changelog

All notable changes to Nuage DB. Versions follow [Semantic Versioning](https://semver.org/).

## Unreleased

### Memory you can count on, and a steadier server

- **The right figures.** The memory shown is the one this server may use: its container's
  limit (cgroup v2 and v1, in a group of its own or not), else the machine's. What is used is
  the container's usage, or this process's where the host gives the limit and nothing else
  (a Heroku dyno): there, the dashboard said that everything was free. The swap is the
  container's; the swap of the whole machine it shares is no longer shown as its own.
  `NUAGE_MEMORY_MB` says the memory by hand. `/api/status`: `memory.used_bytes`, `limited`,
  `cache_budget_bytes`, `reliefs`.
- **One cache budget for all the applications** (`NUAGE_CACHE_MB`, by default a quarter of
  the memory), shared equally and shared again when an application is created or deleted.
  It was an eighth of the memory for each application: six of them could hold three quarters
  of it. `Engine::set_cache_bytes`, `cache_capacity`, `drop_cache`;
  `ServerOptions::cache_budget`.
- **The caches use the memory there is.** With `NUAGE_CACHE_MB=auto` the budget follows the
  memory of the server, whatever its size: a quarter of it at first; every ten seconds a
  thirty-second more while the caches are full and the memory in use would stay under 65 %
  once they have filled (up to six tenths of the memory), a thirty-second less past 78 %.
  Each application has what it holds plus an equal part of what is free
  (`memory::cache_shares`), given again every ten seconds: one busy application takes what
  five nearly empty ones leave, where equal parts kept it to a sixth. What the allocator keeps
  of memory freed since a burst goes back to the system when the caches want room.
  `memory::cache_move`, `ServerOptions::cache_ceiling`, `Engine::cache_used`; `/api/status`:
  `memory.cache_ceiling_bytes`.
- **A memory guard.** Every two seconds; past 85 % in use, the caches are emptied, their
  budget halved (16 MiB at least) and the freed memory returned to the system, while requests
  go on; for five minutes the budget does not grow again, and for an hour it stays under
  three quarters of the one that did not fit. Said in the event log and in the server's log.
- **The allocator** keeps to eight arenas on a server of 2 GiB or less: a quarter less
  memory held under load, for a few percent of throughput.
- **The key-value stores keep to a budget too**: with `NUAGE_KV_MB=auto`, all of them
  together hold an eighth of the memory at most; each has the smaller of its own limit and
  its equal part. `Kv::set_max_bytes`; `ServerOptions::kv_budget`.
- **Threads are bounded**: blocking work on four threads a core (16 to 64, less where the
  system allows few: a Heroku dyno has 256 in all), instead of up to 512; more requests wait
  their turn. PostgreSQL connections, a thread each, are limited
  (`NUAGE_PG_MAX_CONNECTIONS`, 100 by default); the next ones are told
  `too_many_connections` (53300).
- **The upkeep thread survives a failure**: a round that panics is reported, and the
  checkpoints and backups go on.
- The dashboard has a **Memory** card (used, free, of how much), with its details; the
  console's Server page shows the same.
- **Memory reads in gigabytes**: `0.45 GB free`, `0.05 GB / 0.50 GB` used (a gigabyte of
  memory is 1024³ bytes, as memory is sold: a 512 MB dyno shows `0.50 GB`), on the dashboard
  and on the console's Server page; the caches, smaller, in megabytes of the same base. The
  dashboard writes sizes in the units of its language: `Go`, `Mo`, `ko` in French.

### The key-value store in the database, and backups a setting cannot lose

- **The key-value store can live in the database**: `NUAGE_KV_STORAGE=database`, for every
  application of the server. `memory`, today's store, stays the default; any other value stops
  the start. The keys are kept in a tree of the application's own page file, written through
  the same log and group commit as its rows: they survive a restart, are encrypted and are in
  the backups, behind the same `/v1/kv`, the same commands and the same port. The store's
  executor runs unchanged on a scratch copy of the keys a command names, and what it changed
  is written as one log record: a list of commands, or `MULTI` ... `EXEC`, is one commit,
  with `KEYS`, `DBSIZE` or `FLUSHDB` inside it. A write is answered once it is on disk; a read
  takes no lock. Nothing is evicted (`NUAGE_KV_MB` is not used; a value is 64 MiB at most).
  Expiries are dates, so a TTL means the same after a restart, and expired keys leave the
  tree in batches in the background. A write the disk refuses is the server's storage error
  (`500` over HTTP, `-ERR storage: …` on the port), never "no such key". Going back to
  `memory` leaves the keys in the database, found again when `database` comes back: nothing
  is dropped at start. Branches, SQL and the console's tables do not see the keys.
- **The figures say where the keys are**: `storage` (`memory` or `database`) in the `stats`
  of `GET /v1/kv` and in each database's `kv` in `/api/status`. In the database, `max_bytes`
  is 0 and `INFO` says `maxmemory:0` and `maxmemory_policy:noeviction`. A value too large
  answers `413` (`ERR` on the port): `the value is larger than the key-value store takes
  (NUAGE_KV_MB in memory, 64 MiB in the database)`. `openapi.json` describes `storage`, and
  the `500` every `/v1/kv` request may answer in the database. The startup log says
  `key-value stores are kept in each application's database`.
- **A database never writes over another database's backup.** Before every snapshot, the
  destination's `encryption.json` is compared with the data directory's (kind, salt and check
  value). A place that holds another database's backup is refused before anything is written
  or pruned there: `<place> holds the backup of another database (its encryption.json is not
  this one's): nothing was written there`, and the backup shows failing in the console and
  `/api/status`. A destination that is only ahead (two dynos overlapping) is never refused.
  Backups in clear carry no such identity.
- **Backup rounds run one at a time**: the upkeep's round and the console's **Back up now** no
  longer tear the snapshot being copied.
- **A bucket that does not exist is an error** (``GET …: the bucket `x` does not exist (HTTP
  404 NoSuchBucket)``), at startup and in every round, where it read as an empty backup. A
  missing object is still read as missing.
- **S3 requests have time limits**: a request that gets no answer within 60 seconds (an
  upload has one more second for each 64 KiB it sends), or an answer that stops for 60
  seconds, fails with `… did not answer within 60 seconds` or `… stopped answering for 60
  seconds`. A bucket that hangs fails the round, which is tried again, instead of holding the
  backups, the stop or `/v1/files` without end. A restore is never cut while its bytes keep
  coming, whatever its speed.
- **An empty start is said.** When the data directory is empty and the backup holds nothing to
  restore, the server's log and a `warn` event say `the data directory was empty and <place>
  holds no backup: this is a NEW, EMPTY database`, with what to check. A registered
  application whose backup holds nothing gets a line of its own.
- **`NUAGE_RESTORE_REQUIRED=true` refuses it.** With nothing to restore, or no backup set, the
  server stops with exit status 1 and says what to check, before it creates a folder, a header
  or a page file and before anything is sent to a backup. A registered application whose
  directory is empty and whose backup holds nothing is not served either: it is listed as one
  that cannot be opened, and comes back once its backup is complete, or is deleted in the
  console. A new application starts empty. `true`, `1`, `yes`, `false`, `0` and `no` are read;
  any other value stops the start. Set it once a server on an erased disk (Heroku) holds data:
  it is off by default and not preset in `app.json`, since a first start has nothing to
  restore.
- **Documentation**: the READMEs, in English, French and Spanish, say where the key-value
  store lives and how to choose, how to change the backup destination on a disk that is not
  erased and on Heroku (with or without a stop), and what `NUAGE_RESTORE_REQUIRED` does.
  `nuage.env.example` names every setting the server reads, and `app.json` offers the two new
  ones, empty. A test (`tests/docs.rs`) keeps the settings tables of `config.rs` and of the
  three READMEs, the example file and `app.json` in agreement, and the three READMEs with the
  same headings.
- **Rust API**: `Engine::open_existing` (an empty directory must be restored),
  `Engine::restore_required`, `EngineOptions::kv_in_database`, `BackupStatus::started_empty`,
  `S3Store::with_timeout`; `Config::restore_required`. For an application that embeds the
  engine: `EngineOptions` and `BackupStatus` have new public fields (build them with
  `..Default::default()`), a snapshot now refuses a destination that holds another
  database's backup, and `S3Store::get` answers an error for a bucket that does not exist.
  `Engine::open` is unchanged.

### A second backup destination, kept in step

- **Two places at once**: `NUAGE_BACKUP2_DIR`, or `NUAGE_BACKUP2_S3_BUCKET` with
  `_REGION`, `_ENDPOINT`, `_PREFIX`, `_ACCESS_KEY_ID` and `_SECRET_ACCESS_KEY` (else
  `AWS_*`), add a second destination. Each one gets the snapshots and the log segments with
  its own progress: one that does not answer never holds the other back, the log is kept on
  the disk for the first only (a second that is away never fills it), and a second that
  missed segments no longer on the disk gets a whole snapshot before any further segment,
  never one after a hole. A round in which both are due copies the page store once. Every
  application backs up under `apps/<name>/` of both; deleting one empties both.
- **An empty data directory is restored from one destination, whole**: the one that reaches
  furthest in the log, the first on a tie, the second when the first holds nothing. A first
  that does not answer, two destinations that hold different databases, or a second that
  does not answer while the first holds nothing stop the start, and nothing is created; a
  second that does not answer otherwise is said, and the first is used.
- **The status tells both**: `target` reads `A and B`, `last_ok` is true only while the last
  attempt of each succeeded, `last_error` names the second (`second destination (<place>):
  …`), and the last backup's time moves only then: the console and `/app` show it as they
  are. The startup line says where the backups go and how an empty disk is restored.
- **The stop** sends to the first destination of every application before any second one:
  a slow second cannot take the first's last seconds.
- **Settings**: set alone, `NUAGE_BACKUP2_*` is the only destination, so adding the second,
  waiting until it has everything, then removing the first moves the backups without a stop
  (the steps for Heroku are in the README). `NUAGE_BACKUP2_DIR` with
  `NUAGE_BACKUP2_S3_BUCKET`, or the two destinations naming the same place, stop the start.
  Without them, nothing changes: the backup store gets the same calls, in the same order.
- **Rust API**: `MirrorStore` (a pair of stores, `BackupStore::parts`, which
  `PrefixedStore` forwards), `Engine::backup_to` (one destination's round),
  `engine::backup::inspect` and `BackupInfo` (what a location holds, and how far it
  reaches), `BackupStatus::restored_from`; `Config::backups`.

## 0.4.6 (2026-11-15)

### The dashboard shows what an application keeps besides its tables

- Each application's card of `/app`, and its details, show its **files** (how many, and their
  size in the bucket) and its **key-value store** (keys, memory), next to its tables, rows and
  size: an application whose database is small but which keeps gigabytes of files no longer
  looks empty. In English, French and Spanish.
- `/api/status` gives `files` (`{"count", "bytes"}`) for each application, next to `kv`. The
  files are counted in the background, by one listing of the application's folder, at most
  every two minutes while the status is watched (and again after an upload link or a
  deletion): the status itself never waits for the bucket. `files` is `null` without a bucket,
  or until the first count is back.

## 0.4.5 (2026-11-15)

### The key-value store has names of its own

- The key-value store is called what it is, everywhere: the setting of its port is
  **`NUAGE_KV_PORT`** (and `NUAGE_KV_BIND`), next to `NUAGE_KV_MB`; in Rust, `Server::listen_kv`
  and `KvOptions`. `NUAGE_REDIS_PORT`, `NUAGE_REDIS_BIND`, `listen_redis` and `RedisOptions`,
  which 0.4.4 had for a few hours, are gone. Nothing else changes: `/v1/kv`, the commands, and
  the port's wire (RESP 2 and 3, the one Redis clients speak, so `redis-cli` and every
  language's Redis client still connect as they are).
- The READMEs, `openapi.json`, `nuage.env.example` and the logs describe the store by what it
  does (a cache, counters, locks) rather than by another product's name.

## 0.4.4 (2026-11-15)

### A key-value store

- **`/v1/kv`**: each application has a key-value store in memory, next to its database:
  values with an expiry, counters, locks, a cache in front of anything slow. `GET`, `PUT`
  (`?ttl=`, `&nx`, `&xx`, `&keepttl`) and `DELETE /v1/kv/<key>` read, write and delete a value
  as its bytes; `POST /v1/kv/<key>?incr` adds to a counter (`&ttl=` is the expiry of a counter
  it creates: a rate limit in one request), `?expire=` and `?persist` change an expiry;
  `GET /v1/kv` lists the keys and gives the figures; `POST /v1/kv` takes the commands of Redis
  as JSON, one or several in one round trip. A read-only token reads. Nothing touches the
  disk: a restart empties it.
- **The Redis protocol** (`NUAGE_REDIS_PORT=6379`, RESP2 and RESP3): every language's Redis
  client connects as is, a token of the application as its password
  (`redis://default:<token>@host:6379`); with `NUAGE_TLS_CERT` and `NUAGE_TLS_KEY` the same
  port also answers TLS (`rediss://`). Strings and counters (`GET`, `SET` with `EX`, `PX`,
  `NX`, `XX`, `KEEPTTL`, `GET`; `SETNX`, `SETEX`, `GETSET`, `GETDEL`, `GETEX`, `MGET`, `MSET`,
  `APPEND`, `STRLEN`, `INCR`, `DECR`, `INCRBY`, `DECRBY`), keys (`DEL`, `EXISTS`, `EXPIRE`,
  `PEXPIRE`, `TTL`, `PTTL`, `PERSIST`, `TYPE`, `KEYS`, `SCAN`, `DBSIZE`, `FLUSHDB`), `PING`,
  `INFO`, pipelines, and `MULTI` ... `EXEC` (the commands run as one). Tried with `redis-cli`
  and redis-py. Lists, hashes, sets and pub/sub are not there.
- Memory is bounded for each application (`NUAGE_KV_MB`, by default a sixteenth of the memory,
  4 to 256 MiB): past the limit the keys that expired go first, then the least recently used
  ones. `/api/status` gives the figures of each store (keys, bytes, hits, misses, evictions).
- `nuage::kv::Kv` is the store itself, for an application that embeds the engine; Memoire's
  `NuageKv` (0.2.4) is its client: `db.kv()`, `set_for`, `incr_by`, `set_if_absent`,
  `remember`.

### Faster

Measured with `cargo run --release -p nuage --example bench` before (0.4.3) and after, on the
same idle machine (4 cores, 20 000 rows, encrypted, the client and the server on one machine):

| | 0.4.3 | 0.4.4 |
|---|---|---|
| reads by key, 32 clients | 44 300 /s | **89 900 /s** |
| a row by key (Memoire's client) | 0.151 ms | 0.094 ms |
| a row by a unique column | 0.177 ms | 0.112 ms |
| a page of 20 rows | 0.441 ms | 0.241 ms |
| a page of 20 rows, `/v1/sql` | 0.307 ms | 0.177 ms |
| the same, gzip | 0.353 ms | 0.214 ms |
| a durable update | 0.53 ms | 0.48 ms |

- **Reads run where they are.** A read served from the page cache takes a few microseconds;
  handing it to another thread and waiting for its answer cost more than the read. A read now
  runs on its connection's own task while reads of its shape (the same table, the same
  clauses) stay short; a shape that turns out slow (a scan, a cold disk) goes to the blocking
  pool, where it holds up nobody, and comes back when it is short again. Writes still go to the
  pool: they wait for the disk.
- **Answers are written straight as their text**, for `/v1/fetch` (Memoire 0.2.4's
  `wire::rows_to_vec`) and `/v1/sql`, instead of as a JSON tree built, serialized and dropped
  (for `/v1/sql`, one map per row, each column name copied for each value). The same answers:
  tested against the JSON they replace.
- **gzip reuses its compressor**: making one for each answer allocated and cleared a few
  hundred kilobytes, more work than compressing a page of rows.
- **A token is checked with two SHA-256 instead of two for each token of each application**:
  digests are compared as they are (every byte, so the time taken tells nothing).
- The server is built on Memoire 0.2.4 (it was on a revision before 0.2.2): statements are
  read and written without copying what they build.
- The bench measures the key-value store too: 144 000 reads/s over HTTP with 32 clients, a
  read in 0.07 ms.

## 0.4.3 (2026-11-15)

### Files, in a bucket, through signed links

- **`/v1/files`**: each application keeps files of any size (map tiles, photos, archives of
  several gigabytes) in an S3-compatible bucket (Cloudflare R2, AWS S3, MinIO...), in a folder of
  its own. The server lists them (`GET /v1/files`, `?prefix=`) and signs the links: a download
  (`GET /v1/files/<name>`, a `307` to the bucket, `?link` for the link as JSON, `?download=` for
  a file name), an upload (`POST /v1/files/<name>`, then `PUT` the bytes to the link), and it
  deletes them (`DELETE`). **The bytes never go through the server**: a Heroku dyno hands out
  terabytes, and a download resumes where it stopped (`Range`). A read-only token lists and
  downloads; uploading and deleting take a read-write token.
- Set with `NUAGE_FILES_S3_BUCKET`, `NUAGE_FILES_S3_ENDPOINT`, `NUAGE_FILES_S3_REGION` (`auto`
  on R2), `NUAGE_FILES_S3_PREFIX` (`files/`), `NUAGE_FILES_S3_ACCESS_KEY_ID`,
  `NUAGE_FILES_S3_SECRET_ACCESS_KEY` and `NUAGE_FILES_LINK_MINUTES` (60). `/v1/info` says
  `"files": true` when the server keeps files; without a bucket, `/v1/files` answers `501`.
- The S3 client signs links (SigV4 in the query string, checked against the example of the AWS
  documentation) and reads the size, date and ETag of the objects it lists.
- `openapi.json` describes `/v1/files`; the READMEs (English, French, Spanish) have a **Files**
  section, with the setup of Cloudflare R2.

## 0.4.2 (2026-11-15)

### Install on any computer, backups where you choose

- `install.sh` installs on **macOS** too (Apple silicon, Intel): a launchd daemon started with
  the Mac and run as the person who ran `sudo`, the settings in
  `/usr/local/etc/nuage/nuage.env`, the data in `/usr/local/var/nuage`, the log in
  `/usr/local/var/log/nuage.log`.
- `install.ps1` installs on **Windows** 10, 11 and Server, from an administrator PowerShell
  (`irm …/install.ps1 | iex`): `nuage.exe` in `C:\Program Files\Nuage`, the data and the
  settings in `C:\ProgramData\Nuage`, readable by administrators only, a scheduled task that
  starts it with Windows and restarts it, a firewall rule for the local network.
- **The backups are chosen at install**: a folder on another disk (`NUAGE_BACKUP_DIR`), S3-
  compatible storage (`NUAGE_BACKUP_S3_*`) or none (`NUAGE_BACKUP=none`), given in the command
  or asked in the terminal; written in the settings file, kept on upgrades, changed by running
  the installer again with another option. The installer makes only the last folder (an
  external disk that is not plugged in is not replaced by a folder on the computer's own disk)
  and warns when the folder is on the data's disk. `uninstall` and `purge` never delete the
  backups.
- The installers generate `NUAGE_ADMIN_TOKEN` and print the `nuage apps create` line.
- `nuage --env-file <file>` (or `NUAGE_ENV_FILE`): the settings from a file of `KEY=value`
  lines (comments, `export`, quotes, Windows line ends and byte order mark accepted); the
  environment wins over the file. The macOS and Windows services start the server so.
- At startup the server warns when `NUAGE_BACKUP_DIR` is on the same disk as the data.
- `nuage --help` lists the commands.
- CI: the installers run end to end on Linux, macOS and Windows (service, health, first backup
  in the folder, `nuage apps create`, upgrade, uninstall, purge).

### Create an application in one command

- `/api/apps`, the management API: `POST /api/apps {"name", "description"}` creates an
  application, with its own encrypted database, and answers its first token and the two lines
  to paste in its configuration (`env`: `NUAGE_URL`, `NUAGE_TOKEN`). `GET /api/apps` and
  `GET /api/apps/{name}` show them, `POST /api/apps/{name}/tokens` makes another token
  (`{"read_only": true}` for a website), `DELETE /api/apps/{name}/tokens/{id}` revokes one.
  JSON refusals that say what to do (`401`, `403`, `400`, `404`, `409`). Deleting an
  application stays in the console.
- It opens with `NUAGE_ADMIN_TOKEN`, a secret of its own (32 characters at least, `nuage
  genkey`; a shorter one stops the server at startup), compared in constant time. An
  application's token, `NUAGE_TOKEN` included, never opens it. Without it, the API is off.
- `nuage apps create <name> [description]` and `nuage apps list`: the same from a terminal.
  On the server's computer they need nothing else (`sudo nuage apps create shop`): the token
  and the port come from the installer's settings file, and the address shown is its
  `SITE_URL` when it has one; from anywhere else, `NUAGE_URL` and `NUAGE_ADMIN_TOKEN`.
- A name already taken answers `409` with `kind: unique_violation`, which Memoire's
  `NuageAdmin` reads as `Error::UniqueViolation`.
- In `/v1/openapi.json`, the READMEs, `nuage.env.example` and `app.json`.

### The dashboard's cards open

- Every card of `/app` opens its details, live: the requests (the last minute larger, its
  peak and average, each application), the latency and the connections (each application,
  its last connection, the PostgreSQL port), each application (requests, latency, tables,
  rows, sizes, tokens, websites, branches, last connection, recent events, a link to the
  console), each service, the storage (with the memory and the swap), the log and the
  backups; with a sentence that says what each figure means. A sheet from the bottom on a
  phone, a window on a computer; it closes with ✕, Escape or a tap outside.
- `/api/status` gives `postgresql` (port, TLS).

## 0.4.1 (2026-11-14)

### Faster

Measured with `examples/sql_bench.rs` (5,000 rows, medians of three runs), before and
after:

- A row read is decoded into a vector sized once, in a plain loop (it grew value by value
  through an iterator adapter that knows no size). With the next change, a scan of two
  columns (`SELECT id, joue_le FROM main`) takes 0.98 ms instead of 2.11 ms (−54 %), an
  unindexed filter 1.68 ms instead of 2.54 ms (−34 %).
- Plain columns asked in the table's order, half of them or more, are moved to their place
  in each row's own vector, which is then cut, instead of being copied into a new vector
  for every row.
- Rows read by key (an index, `IN (...)`, a subquery) are decoded where they are stored,
  not copied out first, and their keys are built in one buffer, sized once: an index
  matching a third of the table 1.07 ms instead of 1.63 ms (−34 %), `IN (subquery)` over
  every row 8.8 ms instead of 11.6 ms (−24 %), a page of 20 keys by `IN` 24 µs instead of
  30 µs (−20 %).
- JSON answers of the HTTP API are written straight into their buffer
  (`serde_json::to_vec`) instead of through `fmt::Display`, the same bytes: half the time to
  encode a page of 20 rows (6.6 µs instead of 13.5 µs).
- Opening a database after a crash: the records of the log the page store already holds
  are checked as before but not copied out (opening a log of 20,000 commits, most of them
  written to the page store already: 37 ms instead of 76 ms).
- The release build (the `nuage` server, Docker, Heroku) is optimized across crates in one
  code generation unit (`lto = "fat"`, `codegen-units = 1`): Memoire's decoding of each value
  is inlined in the engine. ORDER BY ... LIMIT 20 over the table 1.37 ms instead of 2.07 ms
  (−34 %, with the changes above); the binary is 11.1 MB instead of 14.8 MB. The build takes
  longer (about a minute more).

### Its own page store

- The tables now live in a page file written by Nuage DB itself (`nuage.db`), not in a
  redb file: B-trees of 4 KiB pages, copy-on-write, a CRC-32 on each node checked at every
  read, the prefix the keys of a node share stored once, a node of several pages for a key
  or a value larger than a page.
- A commit changes nodes in memory only (the write-ahead log makes it durable); a
  checkpoint writes the changed nodes to free pages, then a header in the slot the previous
  one does not use: a crash at any point leaves the previous header and all it reaches.
  Changed nodes are also written out as soon as they outweigh a quarter of the cache.
- A reader keeps the root it started with and sees the data as it was, without a lock or a
  read transaction; a page is reused once no header and no reader reaches it. The list of
  free pages is not stored: opening the file walks its branches, so a crash can neither
  leak a page nor hand one out twice.
- Compaction rewrites the file packed, in key order, and reads the copy back before it
  takes the place of the file. The integrity check reads every page: checksums, order of
  the keys, sizes, no page used twice; it no longer waits for readers.
- Encryption is the same (XTS-AES-256, one data unit per page, headers included), and the
  blocks of a page are now encrypted in one call to AES.
- **Coming from 0.4**: a data directory or a backup whose pages are a redb file
  (`nuage.redb`) is imported the first time it is opened, encrypted as it was. The new
  file is read back and compared with the old one, entry by entry, before it takes its
  place: a difference stops the opening and leaves the old file as it was
  (`Engine::imported` says what was imported; the server writes it, and the tables and
  rows of each application, in its startup log, and names an application that could not
  be opened). The old
  file is kept as `nuage.redb.imported`, and a snapshot of this version follows in the
  backup (opening does not wait for it). Before that snapshot, the backup of the earlier
  version is copied whole under `before-import/` of the backup location, where nothing
  prunes it (a server's applications at `before-import/apps/<name>/`: `before-import`
  taken as the backup location has the shape of the location itself, and the earlier
  version started on it finds every application). The database as that version left it
  can be restored from there, by that version or by this one. A backup set aside by an
  earlier try gives way when that version has run again since. This is the `import-redb` Cargo feature (on by default: the server has it), the
  only remaining use of redb: an application that embeds the engine (`default-features =
  false`, with or without `s3`) compiles no redb at all, and asks for the feature if it has
  data of those versions.
  `tests/legacy.rs` opens data directories and backups written by 0.4.0, in clear and
  encrypted, closed normally or left by a crash.
- Snapshots in a backup are named `snapshots/<lsn>.db`.

### Writers at the same time

- The log is no longer synced under the writer lock. A commit is appended to the log and
  applied in memory in its turn, then waits for the log to be on disk before it returns;
  one `fsync` covers every commit appended before it started, so commits made at the same
  time share it.
- Nothing rests on a commit a crash could still undo: plain reads see the last commit that
  is on disk (and never wait for one being written); a transaction that may write goes by
  the latest commits, and each of its answers waits for them to be on disk.
- A SQL script that writes (`POST /v1/sql`, the PostgreSQL protocol outside `BEGIN`,
  `Embedded::sql`) takes its turn from its first statement to its commit instead of running
  as an optimistic transaction retried on a conflict: writers on the same table used to
  collide, wait and, with enough of them, fail with a conflict. A batch (`POST /v1/batch`,
  `Embedded::batch`) that writes takes its turn the same way. Transactions opened by the
  application (`BEGIN`, `db.begin()`) stay optimistic.
- **Fixed**: a transaction could write over a commit that landed between its beginning and
  its first read of the table (the conflict was looked for from that first read, the data
  was as of the beginning). The conflict is now looked for from its snapshot.
- **Fixed**: a row too large for the page store was refused after it had been written to
  the log; it is refused before.
- When the disk fails. A record the log could not write whole is cut from it (the records
  appended after it used to be lost with it at the next start), and so is one that could
  not be applied. A `fsync` of the log that fails is not tried again on the same file:
  commits are refused until the database is opened again, since the system reports such an
  error once and a later `fsync` that succeeds proves nothing. A commit that is in the log
  and applied is no longer answered with an error because writing the page store early
  (much was changed since the last checkpoint) failed. Commits missing in the middle of the
  log are told by an event at opening. Encrypted, a record cut from the log is followed by
  a new segment, under a new key: the number the record had is never sealed twice under
  one key.
- **Fixed**: a filter whose index names a column twice (`CREATE INDEX i ON t (a, a)`) left
  a condition on another column unchecked, for reads, counts, updates and deletes; a
  `LIMIT` cut the rows an aggregate inside an expression counts (`count(*) + 1`) on a
  table larger than what is read at once.
- When the server stops (`SIGTERM`), every application is checkpointed and what it wrote
  since goes to the backup, not the server's own database alone: on a disk that does not
  outlive the process, an application's last seconds of writes were lost at each restart.
- With one snapshot kept (`keep_snapshots: 1`), a snapshot at the same place in the log as
  the one before it could be pruned in its place.
- A long log is replayed without keeping everything it changed in memory: past the same
  limit as for commits, the page store is written on the way.
- The page cache's clock kept a place for every page rewritten at a checkpoint while the
  cache was not full: it grew slowly for as long as the process ran. It is now bounded by
  the pages held.

| `INSERT` with `fsync`, one commit each (`examples/sql_bench.rs`) | Before | After |
|---|---:|---:|
| 1 writer | 4,500 /s | 6,000 /s |
| 8 writers | fails (conflict) | 14,000 /s |
| 32 writers | fails (conflict) | 24,000 /s |

### A lighter protocol

- A statement of Memoire's client (`NuageDb`) no longer describes its tables each time: the
  server says in `/v1/info` (`schemaRefs`) that it keeps schemas by reference, the client
  sends each schema whole once, then a short reference. Describing and reading the schema
  was more than half of the work of a short request, on both sides. A client or a server
  that does not know references works as before (Memoire pinned to `8f7e0cb`).
- A reference is the table's name and half a SHA-256 of its whole description: it names
  one schema, and no client of the server (the holder of a read-only token of another
  application, say) can describe another schema that answers to it.

| Server and client on one machine, encrypted, `fsync` (`examples/bench.rs`) | Before | After |
|---|---:|---:|
| Read by id (p50) | 0.21 ms | 0.12 ms |
| Read by a unique column (p50) | 0.27 ms | 0.19 ms |
| A page of 20 rows through an index (p50) | 0.59 ms | 0.39 ms |
| `POST /v1/sql`, a page of 20 rows (p50) | 0.70 ms | 0.40 ms |
| Reads by id, 32 clients | 15,000 /s | 42,000 /s |

### Faster reads and writes

- A SQL text already parsed is kept (512 of them): applications send the same statements
  with other parameters.
- Rows found by the primary key, or by an index that carries the whole filter, no longer
  go through the evaluator again when they hold exactly the values looked up; a `SELECT`
  of plain columns of one table is answered from the rows read, with no table built in
  memory (`tests/sql.rs` compares both answers on 36 kinds of statements).
- Only the columns a `SELECT` reads are decoded: a long text or a JSON nobody asks for is
  skipped in the stored row.
- Scans and index lookups lend keys and values where they are instead of copying each;
  `COUNT(*)` by an index keeps no key.
- The catalog entry of a table is read in place and found by the table's name, once per
  statement; a table's metadata is shared instead of copied at each write, and the types
  of its columns are worked out once.
- A log record is built, encrypted and framed in one buffer, with a CRC-32 computed eight
  bytes at a time.

| 5,000 rows, embedded (`examples/sql_bench.rs`) | Before | After |
|---|---:|---:|
| `INSERT` in a transaction (per row) | 35 µs | 16 µs |
| `INSERT`, one commit per row | 62 µs | 15 µs |
| `UPDATE … WHERE id = ?`, one commit each | 57 µs | 19 µs |
| `DELETE … WHERE id = ?`, one commit each | 48 µs | 14 µs |
| `SELECT … WHERE id = ?` (primary key) | 9.6 µs | 4.3 µs |
| `SELECT count(*)` | 3.8 µs | 1.5 µs |
| `SELECT count(*) WHERE indexed = ?` (1/3 of the rows) | 570 µs | 8 µs |
| `SELECT id, date WHERE indexed = ?` (1,667 rows) | 3.4 ms | 1.5 ms |
| `SELECT id, date` (the whole table) | 5.7 ms | 1.6 ms |
| A filter no index answers (the whole table read) | 4.0 ms | 1.9 ms |
| `ORDER BY … LIMIT 20` | 4.1 ms | 1.7 ms |
| `WHERE id IN (subquery)`, 5,000 keys | 15 ms | 9.5 ms |
| `SELECT … WHERE id IN (20 keys)` | 44 µs | 23 µs |

Commits are measured without `fsync` (the log is written, not synced): with it, the disk
sets the time of a commit.

### Faster short statements

- Each table's catalog entry is parsed once and kept, keyed by its stored bytes (the same
  bytes always describe the same table, on every branch and in every transaction), with
  its `'static` schema. Every statement used to parse the catalog JSON of its tables, then
  serialize it again to look the schema up: most of the cost of a short statement.
- `COUNT(*)` whose filter is equalities pinning exactly the columns of an index is counted
  from the index, without reading the rows.
- `examples/sql_bench.rs`: embedded SQL timings (`cargo run --release -p nuage
  --no-default-features --example sql_bench -- 5000`).

| 5,000 rows, embedded | Before | After |
|---|---:|---:|
| `INSERT` in a transaction (per row) | 73 µs | 32 µs |
| `SELECT … WHERE id = ?` (primary key) | 37 µs | 8.5 µs |
| `SELECT count(*)` | 29.6 µs | 3.6 µs |
| `SELECT count(*) WHERE indexed = ?` (1/3 of the rows) | 2.73 ms | 0.43 ms |
| `SELECT … WHERE id IN (20 keys)` | 69 µs | 42 µs |

Scans (about 1 µs per row) and subqueries are unchanged.

### SQL in an embedded transaction

- `EmbeddedTransaction::sql(sql, params)`: SQL run inside a transaction opened with
  `Embedded::begin_now`. It reads its own writes, others see nothing before `commit_now`,
  and `rollback_now` (or dropping it) undoes it all: read, decide, then write, atomically.

### The embedded engine in pure Rust

- New Cargo features `server` and `s3` (both on by default, with `site`): without them
  (`default-features = false`), Nuage DB is its embedded engine alone, with no network
  stack and no C or assembly. Its dependency tree has no `ring`, `cc`, `rustls`, `hyper` or
  `tokio`.
- Cryptography from RustCrypto instead of `ring`: AES-256-GCM (`aes-gcm`), HKDF-SHA256
  (`hkdf`), SHA-256 and HMAC (`sha2`, `hmac`), in a new `nuage::hash` module. The outputs are
  the same: databases and backups written by earlier versions open as they are (tested
  against values computed with `ring`, and an encrypted database and backup written by the
  previous version). `ring` remains only as rustls's provider, under `s3` and `server`.
- `nuage::config::engine_from_vars`: the data directory and the engine settings (encryption,
  cache, backups) from variables, without the server. `config::from_vars` builds on it
  (feature `server`). Without `s3`, `NUAGE_BACKUP_S3_BUCKET` is refused rather than ignored.
- Memoire pinned to `074fbde` (its `nuage-wire` feature: the protocol without the client).

### PostgreSQL applications, as they are written

- `SELECT ... FOR UPDATE` (`FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE`, `OF t`,
  `NOWAIT`, `SKIP LOCKED`) is accepted: a transaction already fails at its commit (`40001`)
  when another one changed a table it read.
- `pg_advisory_lock`, `pg_advisory_xact_lock`, their `_shared` and `try_` forms,
  `pg_advisory_unlock`, and `hashtext`/`hashtextextended` (a stable 64-bit key).
- `jsonb_array_elements_text` (and `json_`, and the non-`_text` forms) in a SELECT without
  FROM: `x NOT IN (SELECT jsonb_array_elements_text($1::jsonb)::bigint)` passes a list as one
  JSON parameter.
- A text written by a model into a column of numbers or dates is stored as the column's
  type, as in PostgreSQL (`'48.86'` into a `DOUBLE PRECISION`). Before, a model that writes
  decimals as exact text left text among the numbers, and from that one row on every
  comparison on the column failed (`cannot compare text with f64`).
- `INTERVAL '1 day 2 hours'`, added to or taken from a date or a timestamp (years and months
  on the calendar); `extract(epoch FROM x)`; typed literals `DATE '...'`, `TIMESTAMP '...'`;
  `bool_and`, `bool_or`, `every`. A date compared with a timestamp is the date at midnight
  (it was never equal, less nor greater).
- A `DOUBLE` sent to `/v1/sql` arrives bit for bit (serde_json's `float_roundtrip`): a
  latitude copied from PostgreSQL was read one ULP away.
- **Fixed**: inside `BEGIN`, a driver that prepares its statements (most do) ran an
  `INSERT`, `UPDATE` or `DELETE ... RETURNING` once more than asked: the columns of a
  prepared statement are found by a run with its parameters `NULL`, and in an open
  transaction what that run wrote stayed there (`UPDATE t SET n = n + 1 RETURNING n` added
  2). That run is now taken back.

### Dark theme

- The console has a real dark theme (deep night blue, sky-blue accents, Nuage DB's
  gradient), and a **Light · Dark · Auto** switch next to the languages: the choice is kept (a
  cookie), and Auto follows the device. Before, the dark colours only came with a dark device.
- The website has the same switch (☀ ☾ A): the choice stays in the browser (no cookie), and a
  small script applies it before the page paints.

### Languages, in the same spirit

- English, French, Spanish **or Auto** (the browser's language), as a switch like the theme's,
  in the console and on the website; the choice is kept (the console's cookie, the browser's
  storage on the website, which sets no cookie). On the website, `/` and the pages without a
  language still go to the browser's language, unless a language was chosen: the redirect says
  `lang=auto`, and the page moves to the kept one.

## 0.4.0 (2026-11-13)

### Does my connection work?

- `GET /v1/ping`: the application the token opens, its access (read-write or read-only),
  branch and tables, in one sentence; the 401 says whether the token is missing or opens
  nothing, and where to find one.
- `nuage check <url> <token>`: the same from any machine, with what to check when the server
  does not answer.
- A PostgreSQL login gets the same sentence as a notice (psql prints it).
- The console shows each application's last connection (when, HTTP API, SQL over HTTP or
  PostgreSQL, which client, from where), or that it waits for a first one with the command to
  test; `/api/status` gives `last_connection`. Each new token comes with its test command.

### PostgreSQL protocol: every language's driver

- `NUAGE_PG_PORT` (`5432`): Nuage DB speaks version 3 of PostgreSQL's protocol. psql,
  psycopg 2 and 3, SQLAlchemy, node-postgres, Postgres.js, pgx, database/sql, JDBC, PDO, the
  pg gem, tokio-postgres, Npgsql and everything on libpq connect as is: any user name, a token
  of an application as the password, the database name picks a branch.
- The simple protocol (several statements, one transaction) and the extended one (prepared
  statements, `$1` parameters, text and binary formats); `BEGIN` ... `COMMIT` held across
  messages; `SAVEPOINT`, `ROLLBACK TO`, `RELEASE`; PostgreSQL's SQLSTATE codes; read-only
  tokens read only.
- Types of parameters and result columns come from the columns of the tables (`id = $1`).
- The questions drivers ask (`SET`, `SHOW`, `version()`, `current_schema()`, catalogs) get
  PostgreSQL's answers.
- TLS: `NUAGE_TLS_CERT` and `NUAGE_TLS_KEY`; `NUAGE_PG_BIND`. `Server::listen_pg`,
  `nuage::PgOptions`, `nuage::tls_config`, `Server::new_app_read_only_token`.
- The installer turns it on (port 5432, the firewall), with the domain's certificate copied
  from Caddy every day; Docker and Compose publish 5432; the console shows each
  application's PostgreSQL address.
- `clients/postgres/`: an example per driver, all run in CI.


### Every language

- `clients/`: a complete example for curl, JavaScript, TypeScript, Python, PHP, Go, Ruby,
  Java, Kotlin, C#, Swift, Dart (Flutter), Elixir, Rust, C and Perl, each with a `sql(query, params)` function to copy; `clients/test.sh`
  runs them all against a server, in CI.
- `GET /v1/openapi.json`: the OpenAPI 3.1 description of `/v1/sql`, public, to generate a
  client in any other language.
- The console's **Connect** section shows the request in each language, with the server's
  address.

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
