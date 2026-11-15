# Nuage DB depuis n'importe quel langage

[English](README.md) · **Français** · [Español](README.es.md)

Deux façons, toutes deux testées en CI : le SQL sur HTTP (ci-dessous), et le protocole de
PostgreSQL : le pilote PostgreSQL, l'ORM et les outils de chaque langage se connectent avec un
jeton comme mot de passe, exemples dans [`postgres/`](postgres) (psql, psycopg, SQLAlchemy,
node-postgres, Postgres.js, pgx, database/sql, JDBC, PDO, la gem pg, tokio-postgres, Npgsql).

Nuage DB parle SQL sur HTTP : un `POST /v1/sql`, du JSON en entrée, du JSON en sortie. Tout
langage qui sait envoyer une requête HTTP s'en sert, sans rien installer. Chaque fichier ici est
un programme complet, avec une petite fonction `sql(requête, paramètres)` à copier dans votre
code :

| Langage | Fichier | Le lancer |
|---|---|---|
| Shell | [`curl.sh`](curl.sh) | `sh curl.sh` |
| JavaScript (Node 18+, Deno, Bun, navigateurs) | [`javascript.mjs`](javascript.mjs) | `node javascript.mjs` |
| TypeScript | [`typescript.ts`](typescript.ts) | `bun typescript.ts` |
| Python 3 | [`python.py`](python.py) | `python3 python.py` |
| PHP 7.4+ | [`php.php`](php.php) | `php php.php` |
| Go | [`go.go`](go.go) | `go run go.go` |
| Ruby | [`ruby.rb`](ruby.rb) | `ruby ruby.rb` |
| Java 11+ | [`Nuage.java`](Nuage.java) | `java Nuage.java` |
| C# (.NET) | [`csharp.cs`](csharp.cs) | `dotnet run csharp.cs` |
| Perl | [`perl.pl`](perl.pl) | `perl perl.pl` |
| Rust | [`rust/`](rust/src/main.rs) (SQL ; pour les modèles et les requêtes, le client `NuageDb` de [Memoire](https://github.com/maxlestage/Memoire)) | `cargo run` dans `rust/` |
| C, C++ (libcurl) | [`c.c`](c.c) | `cc c.c -lcurl && ./a.out` |
| Kotlin (JVM, Android) | [`kotlin.kt`](kotlin.kt) | `kotlinc kotlin.kt -include-runtime -d nuage.jar && java -jar nuage.jar` |
| Swift (iOS, macOS, Linux) | [`swift.swift`](swift.swift) | `swift swift.swift` |
| Dart, Flutter | [`dart.dart`](dart.dart) | `dart run dart.dart` |
| Elixir | [`elixir.exs`](elixir.exs) | `elixir elixir.exs` |

Définissez `NUAGE_URL` (par défaut `http://localhost:8080`) et `NUAGE_TOKEN` (le jeton d'une
application : `nuage apps create`, ou l'onglet **Tokens** de la console). Ou, en partant de
rien, avec le jeton de gestion du serveur à la place de `NUAGE_TOKEN` : chaque programme crée
alors d'abord sa propre application (`notes-python`, `notes-go`...), avec une petite fonction
`create_app(nom)` à copier aussi, et travaille dessus avec le jeton reçu :

```bash
NUAGE_URL=https://db.example.com NUAGE_ADMIN_TOKEN=... python3 python.py
# created the application notes-python
# inserted 1
# 1 hello from python
```

[`test.sh`](test.sh) les lance tous ainsi contre un serveur neuf, en CI aussi, et vérifie que
chaque jeton n'ouvre que sa propre application.

## Vérifier la connexion

```bash
curl "$NUAGE_URL/v1/ping" -H "Authorization: Bearer $NUAGE_TOKEN"
# {"ok": true, "message": "Connected to Nuage DB 0.4.6: application `blog`, read and write, ...", ...}
nuage check "$NUAGE_URL" "$NUAGE_TOKEN"     # la même chose, en une ligne, depuis n'importe quelle machine
```

Sans jeton, ou avec un jeton qui n'ouvre rien, le 401 dit ce qui ne va pas. La console montre
la dernière connexion de chaque application.

## Créer une application

```http
POST /api/apps
Authorization: Bearer <le NUAGE_ADMIN_TOKEN du serveur>
Content-Type: application/json

{"name": "boutique", "description": "La boutique"}
```

La réponse, `201`, contient le jeton de la nouvelle application, montré cette seule fois, et les
deux lignes à coller dans sa configuration : `{"application": {...}, "token": "nuage_...", "env":
{"NUAGE_URL": "...", "NUAGE_TOKEN": "..."}}`. Un nom déjà pris donne un `409`
(`unique_violation`), un mauvais jeton de gestion un `401`. `GET /api/apps` liste les
applications, `POST /api/apps/{nom}/tokens` crée un autre jeton (`{"read_only": true}` pour un
site). Le jeton de gestion ouvre toutes les applications du serveur : gardez-le dans un script
de déploiement ou un terminal, jamais dans une application ni dans une page. Voir
[Créer une application en une commande](../README.fr.md#créer-une-application-en-une-commande).

## Le stockage clé-valeur

Chaque application a aussi un stockage clé-valeur (un cache, des sessions, des compteurs, des
verrous), dans la mémoire du serveur ou dans sa base. Tous les langages y accèdent en HTTP,
avec le même jeton :

```bash
curl -X PUT "$NUAGE_URL/v1/kv/session:42?ttl=1800" -H "Authorization: Bearer $NUAGE_TOKEN" -d 'ada'
curl "$NUAGE_URL/v1/kv/session:42" -H "Authorization: Bearer $NUAGE_TOKEN"                    # ada
curl -X POST "$NUAGE_URL/v1/kv/visites?incr&ttl=60" -H "Authorization: Bearer $NUAGE_TOKEN"   # {"value":1}
```

Ou sur le port du stockage, quand le serveur a `NUAGE_KV_PORT=6379` : son protocole est celui
que parlent les clients Redis, le client Redis du langage s'y connecte donc tel quel, le jeton
comme mot de passe (`redis://default:<jeton>@hôte:6379`). Voir
[Stockage clé-valeur](../README.fr.md#stockage-clé-valeur).

## Le protocole

```http
POST /v1/sql
Authorization: Bearer <jeton>
Content-Type: application/json

{"sql": "SELECT id, title FROM posts WHERE category = ? ORDER BY id DESC LIMIT 20", "params": [3]}
```

- **Lignes** : `{"columns": ["id", "title"], "rows": [{"id": 42, "title": "Hello"}]}` ; avec
  `"arrays": true`, chaque ligne est un tableau dans l'ordre de `columns`.
- **Écritures** : `{"rows_affected": 1, "last_insert_id": 7}`.
- **Plusieurs instructions**, séparées par `;` ou en `{"statements": [{"sql", "params"}, ...]}` :
  `{"results": [...]}`, une par instruction, dans une seule transaction (tout ou rien).
- **Paramètres** : `?` dans l'ordre, ou `$1`, `$2`... ; les valeurs ne sont jamais lues comme du
  SQL (pas d'injection).
- **Erreurs** : un statut 4xx ou 5xx avec `{"error": {"kind": "...", "message": "..."}}` ; les
  genres sont listés dans la description OpenAPI. Sur `conflict`, relancez les instructions.
- **Les jetons en lecture seule** lisent et n'écrivent jamais : ceux à mettre dans des pages web.

La description OpenAPI, servie par chaque serveur sur
[`/v1/openapi.json`](../crates/nuage/openapi.json), génère un client pour les langages absents
d'ici (Lua, R, Scala, Haskell, OCaml... : `openapi-generator`, Kiota, Swagger Codegen...), et
s'ouvre dans Postman ou Insomnia.
