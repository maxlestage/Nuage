# Nuage DB from any language

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
application: the console's **Tokens** tab). [`test.sh`](test.sh) runs them all against a fresh
server, in CI too.

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
