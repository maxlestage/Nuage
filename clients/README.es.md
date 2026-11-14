# Nuage DB desde cualquier lenguaje

[English](README.md) · [Français](README.fr.md) · **Español**

Dos formas, ambas probadas en CI: SQL sobre HTTP (abajo), y el protocolo de PostgreSQL: el
driver de PostgreSQL, el ORM y las herramientas de cada lenguaje se conectan con un token como
contraseña, ejemplos en [`postgres/`](postgres) (psql, psycopg, SQLAlchemy, node-postgres,
Postgres.js, pgx, database/sql, JDBC, PDO, la gema pg, tokio-postgres, Npgsql).

Nuage DB habla SQL sobre HTTP: un `POST /v1/sql`, JSON de entrada, JSON de salida. Cualquier
lenguaje que sepa enviar una petición HTTP lo usa, sin instalar nada. Cada archivo de aquí es un
programa completo, con una pequeña función `sql(consulta, parámetros)` para copiar en tu código:

| Lenguaje | Archivo | Ejecutarlo |
|---|---|---|
| Shell | [`curl.sh`](curl.sh) | `sh curl.sh` |
| JavaScript (Node 18+, Deno, Bun, navegadores) | [`javascript.mjs`](javascript.mjs) | `node javascript.mjs` |
| TypeScript | [`typescript.ts`](typescript.ts) | `bun typescript.ts` |
| Python 3 | [`python.py`](python.py) | `python3 python.py` |
| PHP 7.4+ | [`php.php`](php.php) | `php php.php` |
| Go | [`go.go`](go.go) | `go run go.go` |
| Ruby | [`ruby.rb`](ruby.rb) | `ruby ruby.rb` |
| Java 11+ | [`Nuage.java`](Nuage.java) | `java Nuage.java` |
| C# (.NET) | [`csharp.cs`](csharp.cs) | `dotnet run csharp.cs` |
| Perl | [`perl.pl`](perl.pl) | `perl perl.pl` |
| Rust | [`rust/`](rust/src/main.rs) (SQL; para modelos y consultas, el cliente `NuageDb` de [Memoire](https://github.com/maxlestage/Memoire)) | `cargo run` en `rust/` |
| C, C++ (libcurl) | [`c.c`](c.c) | `cc c.c -lcurl && ./a.out` |
| Kotlin (JVM, Android) | [`kotlin.kt`](kotlin.kt) | `kotlinc kotlin.kt -include-runtime -d nuage.jar && java -jar nuage.jar` |
| Swift (iOS, macOS, Linux) | [`swift.swift`](swift.swift) | `swift swift.swift` |
| Dart, Flutter | [`dart.dart`](dart.dart) | `dart run dart.dart` |
| Elixir | [`elixir.exs`](elixir.exs) | `elixir elixir.exs` |

Define `NUAGE_URL` (por defecto `http://localhost:8080`) y `NUAGE_TOKEN` (el token de una
aplicación: `nuage apps create`, o la pestaña **Tokens** de la consola). O, desde cero, con el
token de gestión del servidor en lugar de `NUAGE_TOKEN`: cada programa crea entonces primero su
propia aplicación (`notes-python`, `notes-go`...), con una pequeña función `create_app(nombre)`
para copiar también, y trabaja en ella con el token recibido:

```bash
NUAGE_URL=https://db.example.com NUAGE_ADMIN_TOKEN=... python3 python.py
# created the application notes-python
# inserted 1
# 1 hello from python
```

[`test.sh`](test.sh) los ejecuta todos así contra un servidor nuevo, también en CI, y comprueba
que cada token solo abre su propia aplicación.

## Comprobar la conexión

```bash
curl "$NUAGE_URL/v1/ping" -H "Authorization: Bearer $NUAGE_TOKEN"
# {"ok": true, "message": "Connected to Nuage DB 0.4.1: application `blog`, read and write, ...", ...}
nuage check "$NUAGE_URL" "$NUAGE_TOKEN"     # lo mismo, en una línea, desde cualquier máquina
```

Sin token, o con uno que no abre nada, el 401 dice qué falla. La consola muestra la última
conexión de cada aplicación.

## Crear una aplicación

```http
POST /api/apps
Authorization: Bearer <el NUAGE_ADMIN_TOKEN del servidor>
Content-Type: application/json

{"name": "tienda", "description": "La tienda"}
```

La respuesta, `201`, contiene el token de la nueva aplicación, mostrado esta única vez, y las dos
líneas que pegar en su configuración: `{"application": {...}, "token": "nuage_...", "env":
{"NUAGE_URL": "...", "NUAGE_TOKEN": "..."}}`. Un nombre ya usado da un `409`
(`unique_violation`), un token de gestión incorrecto un `401`. `GET /api/apps` lista las
aplicaciones, `POST /api/apps/{nombre}/tokens` crea otro token (`{"read_only": true}` para un
sitio web). El token de gestión abre todas las aplicaciones del servidor: guárdalo en un script
de despliegue o un terminal, nunca en una aplicación ni en una página. Ver
[Crear una aplicación con un comando](../README.es.md#crear-una-aplicación-con-un-comando).

## El protocolo

```http
POST /v1/sql
Authorization: Bearer <token>
Content-Type: application/json

{"sql": "SELECT id, title FROM posts WHERE category = ? ORDER BY id DESC LIMIT 20", "params": [3]}
```

- **Filas**: `{"columns": ["id", "title"], "rows": [{"id": 42, "title": "Hello"}]}`; con
  `"arrays": true`, cada fila es un array en el orden de `columns`.
- **Escrituras**: `{"rows_affected": 1, "last_insert_id": 7}`.
- **Varias instrucciones**, separadas por `;` o como `{"statements": [{"sql", "params"}, ...]}`:
  `{"results": [...]}`, una por instrucción, en una sola transacción (todo o nada).
- **Parámetros**: `?` en orden, o `$1`, `$2`...; los valores nunca se leen como SQL (sin
  inyección).
- **Errores**: un estado 4xx o 5xx con `{"error": {"kind": "...", "message": "..."}}`; los tipos
  están en la descripción OpenAPI. Ante `conflict`, vuelve a ejecutar las instrucciones.
- **Los tokens de solo lectura** leen y nunca escriben: los que poner en páginas web.

La descripción OpenAPI, servida por cada servidor en
[`/v1/openapi.json`](../crates/nuage/openapi.json), genera un cliente para los lenguajes que no
están aquí (Lua, R, Scala, Haskell, OCaml...: `openapi-generator`, Kiota, Swagger Codegen...), y
se abre en Postman o Insomnia.
