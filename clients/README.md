# Nuage DB from any language

**English** · [Français](README.fr.md) · [Español](README.es.md)

Two ways, both tested in CI: SQL over HTTP (below), and PostgreSQL's protocol: every
language's PostgreSQL driver, ORM and tool connects with a token as its password, examples in
[`postgres/`](postgres) (psql, psycopg, SQLAlchemy, node-postgres, Postgres.js, pgx,
database/sql, JDBC, PDO, the pg gem, tokio-postgres, Npgsql).

Nuage DB speaks SQL over HTTP: one `POST /v1/sql` with JSON in, JSON out. Any language that
can send an HTTP request uses it, with nothing to install. Each file here is a complete
program, with a small `sql(query, params)` function to copy into your code:

| Language | File | Run it |
|---|---|---|
| Shell | [`curl.sh`](curl.sh) | `sh curl.sh` |
| JavaScript (Node 18+, Deno, Bun, browsers) | [`javascript.mjs`](javascript.mjs) | `node javascript.mjs` |
| TypeScript | [`typescript.ts`](typescript.ts) | `bun typescript.ts` |
| Python 3 | [`python.py`](python.py) | `python3 python.py` |
| PHP 7.4+ | [`php.php`](php.php) | `php php.php` |
| Go | [`go.go`](go.go) | `go run go.go` |
| Ruby | [`ruby.rb`](ruby.rb) | `ruby ruby.rb` |
| Java 11+ | [`Nuage.java`](Nuage.java) | `java Nuage.java` |
| C# (.NET) | [`csharp.cs`](csharp.cs) | `dotnet run csharp.cs` |
| Perl | [`perl.pl`](perl.pl) | `perl perl.pl` |
| Rust | [`rust/`](rust/src/main.rs) (SQL; for models and queries, the `NuageDb` client of [Memoire](https://github.com/maxlestage/Memoire)) | `cargo run` in `rust/` |
| C, C++ (libcurl) | [`c.c`](c.c) | `cc c.c -lcurl && ./a.out` |
| Kotlin (JVM, Android) | [`kotlin.kt`](kotlin.kt) | `kotlinc kotlin.kt -include-runtime -d nuage.jar && java -jar nuage.jar` |
| Swift (iOS, macOS, Linux) | [`swift.swift`](swift.swift) | `swift swift.swift` |
| Dart, Flutter | [`dart.dart`](dart.dart) | `dart run dart.dart` |
| Elixir | [`elixir.exs`](elixir.exs) | `elixir elixir.exs` |

Set `NUAGE_URL` (default `http://localhost:8080`) and `NUAGE_TOKEN` (a token of an
application: `nuage apps create`, or the console's **Tokens** tab). Or, from nothing, with the
server's management token instead of `NUAGE_TOKEN`: each program then makes its own application
first (`notes-python`, `notes-go`...), with a small `create_app(name)` function to copy too, and
works on it with the token it got:

```bash
NUAGE_URL=https://db.example.com NUAGE_ADMIN_TOKEN=... python3 python.py
# created the application notes-python
# inserted 1
# 1 hello from python
```

[`test.sh`](test.sh) runs them all that way against a fresh server, in CI too, and checks that
each token opens only its own application.

## Check the connection

```bash
curl "$NUAGE_URL/v1/ping" -H "Authorization: Bearer $NUAGE_TOKEN"
# {"ok": true, "message": "Connected to Nuage DB 0.4.7: application `blog`, read and write, ...", ...}
nuage check "$NUAGE_URL" "$NUAGE_TOKEN"     # the same, as one line, from any machine
```

Without a token, or with one that opens nothing, the 401 says what is wrong. The console shows
each application's last connection.

## Create an application

```http
POST /api/apps
Authorization: Bearer <the server's NUAGE_ADMIN_TOKEN>
Content-Type: application/json

{"name": "shop", "description": "The shop"}
```

The answer, `201`, holds the new application's token, shown this once, and the two lines to
paste in its configuration: `{"application": {...}, "token": "nuage_...", "env": {"NUAGE_URL":
"...", "NUAGE_TOKEN": "..."}}`. A name already taken is a `409` (`unique_violation`), a wrong
management token a `401`. `GET /api/apps` lists the applications, `POST /api/apps/{name}/tokens`
makes another token (`{"read_only": true}` for a website). The management token opens every
application of the server: keep it in a deploy script or a terminal, never in an app or a page.
See [Create an application in one command](../README.md#create-an-application-in-one-command).

## The key-value store

Each application also has a key-value store (a cache, sessions, counters, locks), in the
server's memory or in its database. Every language reaches it over HTTP, with the same token:

```bash
curl -X PUT "$NUAGE_URL/v1/kv/session:42?ttl=1800" -H "Authorization: Bearer $NUAGE_TOKEN" -d 'ada'
curl "$NUAGE_URL/v1/kv/session:42" -H "Authorization: Bearer $NUAGE_TOKEN"                    # ada
curl -X POST "$NUAGE_URL/v1/kv/hits?incr&ttl=60" -H "Authorization: Bearer $NUAGE_TOKEN"      # {"value":1}
```

Or on the store's own port, when the server has `NUAGE_KV_PORT=6379`: its wire is the one
Redis clients speak, so the language's Redis client connects as it is, the token as its
password (`redis://default:<token>@host:6379`). See
[Key-value store](../README.md#key-value-store).

## The protocol

```http
POST /v1/sql
Authorization: Bearer <token>
Content-Type: application/json

{"sql": "SELECT id, title FROM posts WHERE category = ? ORDER BY id DESC LIMIT 20", "params": [3]}
```

- **Rows**: `{"columns": ["id", "title"], "rows": [{"id": 42, "title": "Hello"}]}`; with
  `"arrays": true`, each row is an array in the order of `columns`.
- **Writes**: `{"rows_affected": 1, "last_insert_id": 7}`.
- **Several statements**, separated by `;` or as `{"statements": [{"sql", "params"}, ...]}`:
  `{"results": [...]}`, one per statement, in one transaction (all or nothing).
- **Parameters**: `?` in order, or `$1`, `$2`...; values are never parsed as SQL (no
  injection).
- **Errors**: a 4xx or 5xx status with `{"error": {"kind": "...", "message": "..."}}`; the
  kinds are listed in the OpenAPI description. On `conflict`, run the statements again.
- **Read-only tokens** read and never write: the ones to put in web pages.

The OpenAPI description, served by every server at
[`/v1/openapi.json`](../crates/nuage/openapi.json), generates a client for the languages not
listed here (Lua, R, Scala, Haskell, OCaml...: `openapi-generator`, Kiota, Swagger Codegen...), and opens in Postman or Insomnia.
