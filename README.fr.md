<div align="center">

[![Nuage DB](crates/nuage-site/static/og-image.png)](https://github.com/maxlestage/Nuage)

# Nuage DB

**La base de données de Memoire, en pur Rust. Intégrée à votre application ou en serveur, avec le même code.**

[![CI](https://github.com/maxlestage/Nuage/actions/workflows/ci.yml/badge.svg)](https://github.com/maxlestage/Nuage/actions/workflows/ci.yml)
[![Licence : propriétaire](https://img.shields.io/badge/licence-propri%C3%A9taire-2a2a2e.svg)](#licence)
[![Déployer sur Heroku](https://www.herokucdn.com/deploy/button.svg)](https://heroku.com/deploy?template=https://github.com/maxlestage/Nuage)

[English](README.md) · **Français** · [Español](README.es.md)

</div>

---

Nuage DB garde vos tables sur disque derrière un journal d'écriture, crée en un instant des
branches copy-on-write d'une base, se sauvegarde vers un second emplacement et s'en restaure
au démarrage, et sert le tout en HTTP(S) avec une console d'administration et un tableau de
bord en direct. C'est la base de données de [Memoire](https://github.com/maxlestage/Memoire),
l'ORM Rust : les mêmes modèles et les mêmes requêtes tournent dessus en mode intégré pendant
le développement et les tests, et sur un serveur Nuage DB en production, sans changer une
ligne.

## Fonctionnalités

| | |
|---|---|
| **Chiffré au repos** | Tout ce qui est stocké est chiffré (`NUAGE_ENCRYPTION_KEY`) : les pages en XTS-AES-256, chaque enregistrement du journal en AES-256-GCM, et les sauvegardes, qui en sont des copies. Clés dérivées par application (HKDF, ou Argon2id depuis une phrase de passe). |
| **Plusieurs applications** | Un serveur, une base par application : ses jetons, ses clés, ses sauvegardes. Le jeton qu'envoie une application choisit sa base. La console et `/app` les montrent côte à côte. |
| **Pages sur disque** | Tables et index dans un seul fichier de pages en arbre B écrit par Nuage DB lui-même (copie sur écriture, somme de contrôle sur chaque nœud), avec un cache de pages borné (`NUAGE_CACHE_MB`) : les données ne sont pas limitées par la mémoire. Les recherches par clé primaire et par index ne lisent que les lignes utiles. |
| **Journal d'écriture** | Chaque commit est ajouté au journal et synchronisé sur disque avant d'être rendu ou montré à un lecteur ; les commits faits en même temps partagent une synchronisation. Checkpoints, reprise après crash, CRC-32 sur les enregistrements du journal et sommes de contrôle sur les pages. |
| **Sémantique de Memoire** | Les requêtes passent par l'évaluateur de Memoire, sur les lignes lues sur disque : Nuage DB se comporte exactement comme le moteur en mémoire de Memoire, quelle que soit la taille des données. |
| **Transactions** | Lectures sur instantané, commits optimistes (`Error::Conflict`, rejoués par `db.transaction(...)`), transactions interactives en HTTP, lots tout-ou-rien. |
| **Branches** | Une copie complète de la base pour une préversion, un test ou une expérience, créée en un instant : elle partage les pages de son parent et ne stocke que ses propres changements. |
| **Sauvegardes restaurées au démarrage** | Instantanés et segments de journal, vers un dossier ou un stockage compatible S3, à un endroit ou à deux tenus à jour ensemble. Un répertoire de données vide est restauré automatiquement au démarrage : les hébergeurs à disque éphémère fonctionnent. Une base n'écrit jamais sa sauvegarde par-dessus celle d'une autre. |
| **Stockage clé-valeur** | Chaque application a un stockage, dans la mémoire du serveur ou dans sa propre base (`NUAGE_KV_STORAGE`) : valeurs avec expiration, compteurs, verrous, cache, en HTTP (`/v1/kv`) et sur un port à lui ([Stockage clé-valeur](#stockage-clé-valeur)). |
| **Fichiers** | Chaque application garde des fichiers de toute taille (tuiles de carte, photos, archives) dans un bucket compatible S3 comme Cloudflare R2 : le serveur les liste et signe les liens, les octets vont directement du client au bucket ([Fichiers](#fichiers)). |
| **Serveur** | Un seul binaire, configuré par variables d'environnement : le protocole de la base (`/v1`), le statut en direct (`/api/status`, `/api/status/stream`), la console (`/admin`) et le site avec son tableau de bord en direct (`/app`), sur un seul port. |
| **Client WebAssembly** | `memoire_orm::NuageDb` parle HTTP(S) : en natif avec Tokio, et dans le navigateur via `fetch`. |
| **Entretien automatique** | Un fil d'arrière-plan fait les checkpoints, envoie les sauvegardes et compacte le fichier. |

## Deux modes, une API

Nuage DB s'utilise avec l'API de [Memoire](https://github.com/maxlestage/Memoire) : modèles,
constructeur de requêtes, relations, transactions et migrations.

```toml
[dependencies]
# Le moteur, le mode intégré et le serveur, sans le site.
nuage = { git = "https://github.com/maxlestage/Nuage", default-features = false, features = ["server"] }
# Le même Memoire que Nuage DB (la feature `nuage` est le client d'un serveur).
memoire-orm = { git = "https://github.com/maxlestage/Memoire", rev = "8f7e0cb", features = ["nuage"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

| Feature Cargo | Ce qu'elle ajoute | Par défaut |
|---|---|---|
| *(aucune)* | le moteur intégré (`nuage::Embedded`), le SQL, le chiffrement, les sauvegardes dans un dossier : **pur Rust**, sans pile réseau, ni C ni assembleur (RustCrypto) | toujours |
| `s3` | les sauvegardes vers un stockage compatible S3 (HTTP/1.1 sur rustls) | oui |
| `server` | le serveur (`/v1`, protocole PostgreSQL, console, état en direct), `nuage::Db::Remote` et le binaire `nuage` ; inclut `s3` | oui |
| `site` | le site et le tableau de bord `/app`, servis par le binaire ; inclut `server` | oui |
| `import-redb` | lit une fois le stockage de pages de Nuage DB jusqu'à 0.4 (`nuage.redb`), pour amener un dossier de données ou une sauvegarde de ces versions à celle-ci (voir [En venant de la version 0.4](#en-venant-de-la-version-04)) ; le seul usage de redb | oui |

`default-features = false`, c'est le moteur intégré seul ; ajoutez `features = ["s3"]` pour les
sauvegardes S3 (la configuration d'une base intégrée avec elles :
`nuage::config::engine_from_vars`).

### Intégré : un répertoire sur disque

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
    let db = Embedded::open("data")?; // créé s'il n'existe pas
    db.create_table_if_not_exists::<Note>().await?;
    Note { id: 0, text: "bonjour".into() }.insert(&db).await?;
    println!("{:?}", Note::query().all(&db).await?);
    Ok(())
}
```

### Serveur : le même code en HTTP(S)

```rust
let db = NuageDb::connect("https://mon-nuage.herokuapp.com", token).await?;
Note::query().filter(Note::TEXT.contains("bonjour")).all(&db).await?;
```

### Un seul réglage pour les deux

```rust
// NUAGE_URL=data                                    -> intégré, dans ./data
// NUAGE_URL=https://db.example.com NUAGE_TOKEN=...  -> un serveur
let db = nuage::Db::from_env().await?;
```

### Branches

```rust
db.create_branch("preview-42", None).await?;         // instantané, quelle que soit la taille
let preview = db.on_branch("preview-42");
Note { id: 0, text: "seulement dans la préversion".into() }.insert(&preview).await?;
db.reset_branch("preview-42").await?;                // oublie ses changements
db.delete_branch("preview-42").await?;
```

## Chiffrement

Nuage DB chiffre tout ce qu'il stocke (AES-256, HKDF-SHA256, Argon2id) :

| Quoi | Comment |
|---|---|
| pages (`nuage.db` : tables, index, branches, liste des applications) | XTS-AES-256 (IEEE 1619), le mode du chiffrement de disque : les pages gardent leur place et leur taille |
| journal d'écriture (`wal/*.log`) | AES-256-GCM par enregistrement, sous une clé propre à chaque segment (sel aléatoire), le LSN comme nonce |
| sauvegardes (dossier ou S3) | copies des fichiers ci-dessus, chiffrées de même, avec l'en-tête du répertoire |

`NUAGE_ENCRYPTION_KEY` est une clé (`openssl rand -hex 32`) ou une phrase de passe. Chaque
répertoire de données a un en-tête, `encryption.json`, avec un sel aléatoire et une valeur de
contrôle (jamais la clé) : sa clé racine est HKDF(clé, sel), ou Argon2id(phrase, sel)
(64 Mio, 3 passes), et les clés des pages, du journal et la valeur de contrôle en dérivent.
Chaque application a son répertoire, donc son sel et ses clés. Une mauvaise clé est refusée
au démarrage, avant toute lecture. Une base ou une sauvegarde stockée en clair par une
version précédente est chiffrée à l'ouverture avec une clé, et la sauvegarde en clair est
remplacée. **Gardez la clé** : sans elle, ni les données ni les sauvegardes ne sont lisibles.
Le mode intégré chiffre aussi, avec
`EngineOptions { encryption: Some(Encryption::parse(clé)?), .. }` ou `NUAGE_ENCRYPTION_KEY`
avec `nuage::Db::from_env()`.

### Contre les ordinateurs quantiques

Ce que Nuage DB stocke est chiffré par des algorithmes symétriques et des clés de 256 bits
(AES-256, HKDF-SHA256, Argon2id, SHA-256 pour les empreintes des jetons) : un ordinateur
quantique ne fait que diviser par deux la force d'une telle clé (Grover), il reste 128 bits. Ce
qu'un ordinateur quantique casse, c'est l'échange de clés publiques de TLS : une conversation
enregistrée aujourd'hui pourrait être déchiffrée plus tard. Chaque connexion que Nuage DB
chiffre lui-même convient donc de ses clés avec **X25519MLKEM768** en premier, un hybride de
X25519 et de ML-KEM-768, l'encapsulation de clé post-quantique de la norme FIPS 203, sûr tant que
l'un des deux tient (aws-lc-rs, dont le ML-KEM est validé FIPS 140-3, via rustls) :

- les ports PostgreSQL et clé-valeur avec `NUAGE_TLS_CERT` et `NUAGE_TLS_KEY` ;
- **HTTPS sur un port à lui** avec `NUAGE_HTTPS_PORT` (le même certificat) : la console, son mot
  de passe, les jetons et l'API chiffrés de bout en bout, sans proxy devant ;
- les sauvegardes vers S3, `nuage check`, `nuage apps` et `nuage settings`, et Nuage DB Éditeur,
  en tant que clients (quand l'autre côté le parle ; sinon X25519, comme avant).

Un client plus ancien (un pilote PostgreSQL sur OpenSSL avant 3.5) obtient X25519 ou P-256, sauf
avec `NUAGE_TLS_POST_QUANTUM=required` : seul TLS 1.3 avec un échange post-quantique est alors
accepté. Les jetons que Nuage DB fabrique font 256 bits aléatoires. Sur Heroku, HTTPS s'arrête au
routeur d'Heroku, que Nuage DB ne contrôle pas : pour un site post-quantique là-bas, mettez devant
un service qui parle X25519MLKEM768 (Cloudflare le fait par défaut), ou utilisez le port
PostgreSQL avec TLS.

### En venant de la version 0.4

Jusqu'à la 0.4, les pages étaient un fichier [redb](https://docs.rs/redb), `nuage.redb`.
Ce sont maintenant celles de Nuage DB (`nuage.db`). Rien à faire : un répertoire de données
d'une version antérieure, ou une sauvegarde d'une telle version restaurée dans un répertoire
vide, est importé à la première ouverture par cette version, chiffré comme il l'était, et le
journal écrit depuis son dernier point de contrôle est rejoué par-dessus. Avant de prendre sa
place, le nouveau fichier est relu et comparé à l'ancien, entrée par entrée : le moindre écart
arrête l'ouverture et laisse l'ancien fichier tel quel. Le serveur écrit dans son journal de
démarrage ce que contient chaque application (tables, lignes) et ce qui a été importé.
L'ancien fichier reste à côté du nouveau sous le nom `nuage.redb.imported` : supprimez-le une fois vos données
vérifiées. Quand des sauvegardes sont configurées, un instantané de cette version suit dans
l'instant : la restauration suivante ne passe plus par l'import. Juste avant, la sauvegarde
de l'ancienne version (son manifeste, son instantané, ses segments de journal et son en-tête
de chiffrement) est copiée sous `before-import/` dans l'emplacement de sauvegarde, où rien
ne la purge : la base du serveur dans `before-import/`, chaque application dans
`before-import/apps/<nom>/`, la forme de l'emplacement de sauvegarde lui-même. Revenir en
arrière, c'est démarrer l'ancienne version sur un répertoire de données vide avec
`before-import` comme emplacement de sauvegarde (`NUAGE_BACKUP_S3_PREFIX` ou
`NUAGE_BACKUP_DIR` suivi de `before-import/`) : elle y trouve chaque application. Cette
version restaure aussi depuis là. Supprimez-la une fois vos données vérifiées.

La lecture de l'ancien fichier est la fonctionnalité Cargo `import-redb`, active par défaut :
le serveur l'a. Une application qui embarque le moteur (`default-features = false`, avec ou
sans `s3`) ne contient plus redb du tout et refuse un tel répertoire avec un message qui le
dit ; si elle a des données ou des sauvegardes de ces versions, elle ajoute
`features = ["import-redb"]`. Une fois toutes les bases ouvertes par cette version,
la fonctionnalité peut être retirée.

## Plusieurs applications

Un serveur accueille plusieurs applications, chacune avec sa base :

- la **première application** est la base du serveur (`NUAGE_NAME`, données à la racine de
  `NUAGE_DATA_DIR`, jeton `NUAGE_TOKEN`) : un déploiement existant garde ses données telles
  quelles ;
- les **autres** se créent dans la console (`/admin`, **New application**), ou d'une
  requête à l'API de gestion (`nuage apps create`, voir plus bas) : une base dans
  `apps/<nom>/`, chiffrée sous ses propres clés, sauvegardée sous `apps/<nom>/`, et un premier
  jeton, affiché une seule fois (le serveur n'en garde que l'empreinte SHA-256) ;
- le **jeton** qu'envoie une application choisit sa base : `NuageDb::connect(url, jeton)` ne
  change pas, et une application ne voit jamais les données d'une autre, ni ses transactions ;
- leur liste est dans les métadonnées de la première application : journalisée, chiffrée,
  sauvegardée et restaurée avec elle, puis chaque application restaure sa propre sauvegarde.

`Server::create_app`, `new_app_token`, `revoke_app_token`, `delete_app` et `apps` font de
même depuis Rust.

### Créer une application en une commande

L'API de gestion, `/api/apps`, crée les applications et leurs jetons en une requête HTTP :
depuis un script, une CI, une étape de déploiement, sans le formulaire de la console. Elle
s'ouvre avec un jeton à elle, `NUAGE_ADMIN_TOKEN`.

1. **L'activer**, une fois, sur le serveur : `NUAGE_ADMIN_TOKEN=$(nuage genkey)` (32
   caractères au moins ; plus court, le serveur refuse de démarrer et dit pourquoi). Les
   installateurs en mettent déjà un. Sur Heroku : l'app du serveur → **Settings** → **Config
   Vars**. Le journal de démarrage dit `the management API (/api/apps) is on`.
2. **Créer une application**. Sur l'ordinateur du serveur, rien d'autre : `nuage` lit le
   jeton et le port dans le fichier de réglages de l'installateur (d'où `sudo` ; sur Windows,
   un PowerShell en administrateur), et montre `SITE_URL` comme adresse quand le fichier l'a :

   ```bash
   sudo nuage apps create boutique "La boutique"
   # NUAGE_URL=http://127.0.0.1:8080
   # NUAGE_TOKEN=nuage_3f9a12c0…
   ```

   D'ailleurs, donnez-lui le serveur et son jeton de gestion :

   ```bash
   export NUAGE_URL=https://mon-nuage.herokuapp.com NUAGE_ADMIN_TOKEN=…
   nuage apps create boutique "La boutique"
   # NUAGE_URL=https://mon-nuage.herokuapp.com
   # NUAGE_TOKEN=nuage_3f9a12c0…
   nuage apps list
   # nuage       1 token(s)
   # boutique    1 token(s)   La boutique
   ```

   ou avec n'importe quel client HTTP :

   ```bash
   curl https://mon-nuage.herokuapp.com/api/apps \
     -H "Authorization: Bearer $NUAGE_ADMIN_TOKEN" \
     -d '{"name": "boutique", "description": "La boutique"}'
   ```

   ```json
   {
     "application": { "name": "boutique", "description": "La boutique", "first": false, "created_at": 1794690000000,
                      "tokens": [{ "id": "3f9a12c0", "created_at": 1794690000000, "read_only": false }],
                      "env_token": false, "origins": [] },
     "token": "nuage_3f9a12c0…",
     "env": { "NUAGE_URL": "https://mon-nuage.herokuapp.com", "NUAGE_TOKEN": "nuage_3f9a12c0…" },
     "message": "Application `boutique` created, with its own encrypted database. Copy its token now: it is not shown again."
   }
   ```

3. **Brancher l'application** : les deux lignes de `env` dans sa configuration (sur Heroku,
   ses Config Vars). Elle se connecte et crée ses tables à son premier démarrage.

Depuis Rust, `memoire_orm::NuageAdmin` (la fonction `nuage` de Memoire) fait de même :
`NuageAdmin::new(url, jeton_de_gestion)?.create_app("boutique", "La boutique").await?` donne le
jeton et `env()`, les deux lignes. Dans un projet [Paquets](https://github.com/maxlestage/Paquets),
`paquets nuage create boutique` crée l'application et écrit les deux lignes dans `.env`.

| Méthode | Chemin | Corps | Réponse |
|---|---|---|---|
| `GET` | `/api/apps` | | `200 {"applications": [...]}`, la première d'abord, puis par nom |
| `POST` | `/api/apps` | `{"name": "boutique", "description": "…"}` (description facultative) | `201` l'application, son `token` et `env` ; `409` le nom est pris ; `400` un nom invalide (1 à 63 de `a-z 0-9 - _`) |
| `GET` | `/api/apps/{nom}` | | `200` l'application ; `404` |
| `POST` | `/api/apps/{nom}/tokens` | `{"read_only": true}` (facultatif, `false` par défaut) | `201 {"application", "token", "read_only", "env"}` |
| `DELETE` | `/api/apps/{nom}/tokens/{id}` | | `200 {"revoked": true}` ; `404` pas de tel jeton |
| `POST` | `/api/apps/{nom}/origins` | `{"origins": ["https://site.fr", "http://localhost:3000"]}`, la liste entière ; `[]` les retire | `200` l'application, avec ses `origins` ; `400` pas une origine (schéma et hôte, sans chemin, ou `*`) |
| `GET` | `/api/status` | | `200` le statut en direct de toutes les applications, comme une session de la console le voit |

Un refus est du JSON, `{"error": {"kind", "message"}}`, avec un message qui dit quoi faire :
`401` sans le jeton de gestion ou avec un mauvais, `403` quand `NUAGE_ADMIN_TOKEN` n'est pas
défini, `400`, `404`, `405`, `409`. La description complète est dans
[`/v1/openapi.json`](crates/nuage/openapi.json).

Ce qui la protège :

- **Un secret à elle**, ni le mot de passe de la console (un mot de passe se tape, donc se
  devine), ni le jeton d'une application : un jeton ouvre sa base et rien d'autre, le
  `NUAGE_TOKEN` de la première application compris. Il est comparé en temps constant.
- **Un jeton ne se voit qu'une fois**, dans la réponse qui le crée : le serveur n'en garde
  que l'empreinte SHA-256, et la liste montre les jetons par leur identifiant (leurs premiers
  caractères).
- **Pas de suppression** : supprimer une application efface ses données et ses
  sauvegardes ; cela reste dans la console, derrière son nom tapé à nouveau.
- Gardez `NUAGE_ADMIN_TOKEN` avec les autres secrets du serveur, jamais dans une application
  ni dans une page. Pour le changer, donnez-lui une nouvelle valeur : l'ancienne ne sert plus
  au démarrage suivant.

## SQL

Nuage DB parle SQL (`POST /v1/sql`, l'onglet **Query** de la console, `Embedded::sql` en Rust) :

| | |
|---|---|
| **Lire** | `SELECT [DISTINCT]`, `JOIN`, `LEFT JOIN`, `CROSS JOIN` (`ON`, `USING`), `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY ... NULLS FIRST/LAST`, `LIMIT`/`OFFSET`, `UNION [ALL]`, `INTERSECT`, `EXCEPT`, sous-requêtes (`IN`, `EXISTS`, scalaires, corrélées, dans `FROM`) |
| **Expressions** | `CASE`, `CAST` et `::`, `\|\|`, `+ - * / %`, `LIKE`, `ILIKE`, `BETWEEN`, `IN`, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM` |
| **Fonctions** | `count`, `sum`, `avg`, `min`, `max`, `total`, `group_concat`/`string_agg`, `array_agg` (avec `DISTINCT`) ; `lower`, `upper`, `length`, `trim`, `substr`, `replace`, `instr`, `lpad`, `concat`, `coalesce`, `nullif`, `iif`, `greatest`, `least`, `abs`, `round`, `floor`, `ceil`, `power`, `sqrt`, `random`, `now`, `current_date`, `date`, `extract`, `strftime`, `json_extract`, `json_object`, `json_array`, `gen_random_uuid`... |
| **Écrire** | `INSERT ... VALUES / SELECT / DEFAULT VALUES`, `ON CONFLICT ... DO NOTHING / DO UPDATE SET ... excluded.x`, `INSERT OR IGNORE`, `REPLACE INTO`, `ON DUPLICATE KEY UPDATE`, `UPDATE ... SET col = <expression>`, `DELETE`, `RETURNING` |
| **Schéma** | `CREATE TABLE [IF NOT EXISTS]` (types de PostgreSQL, MySQL et SQLite ; `PRIMARY KEY`, `SERIAL`/`AUTOINCREMENT`, `NOT NULL`, `UNIQUE`, `DEFAULT`, `REFERENCES`, `UNIQUE (a, b)`), `CREATE TABLE ... AS SELECT`, `CREATE [UNIQUE] INDEX` (construit sur les lignes déjà là), `DROP INDEX`, `ALTER TABLE ... ADD COLUMN`, `DROP TABLE`, `EXPLAIN` |

Une requête que les instructions de Memoire savent exprimer s'exécute comme l'une d'elles, par
les index, en s'arrêtant à la fin de la page ; le reste (fonctions, `CASE`, sous-requêtes,
`UNION`) est calculé par la couche SQL sur les lignes trouvées par les index, avec des
jointures par hachage. Absents : jointures `RIGHT`/`FULL`, `WITH`, fonctions de fenêtre, vues,
déclencheurs, clé primaire sur plusieurs colonnes, changement de clé primaire. Une table créée
sans clé primaire reçoit une colonne `rowid`.

## Tous les langages

Nuage DB parle SQL en HTTP (`POST /v1/sql`, du JSON à l'aller et au retour) : tous les
langages s'en servent, sans rien installer. [`clients/`](clients) contient un exemple complet,
testé sur un vrai serveur en CI, pour curl, JavaScript, TypeScript, Python, PHP, Go, Ruby,
Java, Kotlin, C#, Swift, Dart (Flutter), Elixir, Rust, C et Perl ; la console montre la même requête dans chaque langage avec l'adresse du
serveur. Chaque serveur décrit son API à `/v1/openapi.json` (OpenAPI 3.1) pour générer un
client dans n'importe quel autre langage. Rust a aussi le client natif, `NuageDb`. Et
chaque pilote PostgreSQL se connecte aussi : voir plus bas.

**Ma connexion marche-t-elle ?** `GET /v1/ping` répond quelle application le jeton ouvre :
`curl https://db.example.com/v1/ping -H "Authorization: Bearer <jeton>"` donne
`Connected to Nuage DB 0.4.11: application blog, read and write, branch main, 3 table(s).`,
et sans jeton, ou avec un jeton qui n'ouvre rien, il dit ce qui ne va pas. Depuis n'importe
quelle machine : `nuage check <url> <jeton>`. Une connexion PostgreSQL reçoit la même phrase
en avis (psql l'affiche), et la page de chaque application dans la console montre sa dernière
connexion (quand, comment, quel client), ou qu'elle attend la première.

## Protocole PostgreSQL

Nuage DB parle aussi le protocole de PostgreSQL (`NUAGE_PG_PORT=5432` ; l'installateur et
Docker l'activent) : **le pilote PostgreSQL, l'ORM et l'outil de chaque langage s'y
connectent tels quels**, avec un nom d'utilisateur quelconque et **un jeton d'application
comme mot de passe** ; le nom de base choisit une branche (le nom de l'application, ou `main` :
la principale).

```bash
PGPASSWORD=<jeton> psql "host=db.example.com port=5432 user=app dbname=nuage sslmode=require"
# postgresql://app:<jeton>@db.example.com:5432/nuage?sslmode=require
```

Testé en CI avec psql, psycopg 2 et 3, SQLAlchemy, node-postgres, Postgres.js, pgx,
database/sql de Go, JDBC, PDO, le gem pg, tokio-postgres et Npgsql : exemples dans
[`clients/postgres/`](clients/postgres). Requêtes préparées, paramètres `$1` (leur type vient
des colonnes qu'ils rencontrent), valeurs texte et binaires, `BEGIN` ... `COMMIT` d'un
message à l'autre, savepoints, codes d'erreur de PostgreSQL (`40001` sur un conflit
d'écriture : relancez la transaction). `SELECT ... FOR UPDATE` (`SKIP LOCKED`, `NOWAIT`, `FOR SHARE`) et les verrous consultatifs (`pg_advisory_xact_lock`, `hashtextextended`) sont acceptés : une transaction échoue déjà à son commit (`40001`) si une autre a modifié une table qu'elle a lue. `x IN (SELECT jsonb_array_elements_text($1::jsonb)::bigint)` prend une liste en un seul paramètre JSON. TLS avec `NUAGE_TLS_CERT` et `NUAGE_TLS_KEY`
(l'installateur prend le certificat du domaine). Le SQL est celui de Nuage DB (ci-dessus) ;
les catalogues système (`pg_catalog`) n'y sont pas : les outils qui les parcourent (pgAdmin)
montrent peu de chose, et Npgsql prend `Server Compatibility Mode=NoTypeLoading`. Heroku ne
route que le HTTP : là, utilisez `/v1/sql`.

## Sites web

Tout site peut utiliser la base d'une application, depuis son serveur ou depuis ses pages :

- **`POST /v1/sql`**, depuis n'importe quel langage : du SQL complet, paramètres `?` (ou
  `$1`), des valeurs jamais lues comme du SQL (pas d'injection). Les lignes reviennent en
  objets JSON (`"arrays": true` pour des tableaux), un insert renvoie son `last_insert_id`.
  Plusieurs instructions (séparées par `;`, ou `"statements": [...]`) forment une seule
  transaction, tout ou rien.
- **Depuis le navigateur** : ajoutez le site dans la console (**Settings → Websites** :
  `https://site.fr`, `http://localhost:3000`, ou `*`), ou avec `POST /api/apps/{nom}/origins`
  et le jeton de gestion, et ses pages peuvent appeler `/v1` (CORS). La requête préalable du
  navigateur est mise en cache un jour : un aller-retour, une seule fois.
- **Jetons en lecture seule** (**Tokens → New read-only token**) : un jeton dans une page est
  public, les pages reçoivent donc un jeton qui lit sans jamais écrire ; gardez les jetons
  lecture-écriture sur des serveurs.

```js
const res = await fetch("https://mon-nuage.herokuapp.com/v1/sql", {
  method: "POST",
  headers: { "Authorization": "Bearer " + JETON_LECTURE, "Content-Type": "application/json" },
  body: JSON.stringify({ sql: "SELECT id, title FROM posts WHERE category = ? ORDER BY id DESC LIMIT 20", params: [3] }),
});
const { rows } = await res.json(); // [{ "id": 42, "title": "…" }, …]
```

```bash
curl https://mon-nuage.herokuapp.com/v1/sql -H "Authorization: Bearer $NUAGE_TOKEN" \
  -d '{"sql": "INSERT INTO messages (name, text) VALUES (?, ?)", "params": ["Ada", "Bonjour"]}'
```

Vitesse, mesurée sur la machine du serveur (`cargo run --release -p nuage --example bench`,
données chiffrées, 20 000 lignes, 4 cœurs) : une ligne par clé ou par colonne unique en
**0,1 ms**, une page de 20 lignes par un index en **0,2 ms**, une mise à jour durable (fsync)
en **0,5 ms**, **plus de 85 000 lectures/s** avec 32 clients (plus de 140 000 sur le stockage
clé-valeur). Les réponses JSON de 1 Kio ou plus sont
compressées en gzip pour les clients qui l'acceptent. Ce que ressent un visiteur, c'est
surtout le réseau : choisissez la région de l'hébergeur la plus proche de lui, et un dyno qui
ne se met jamais en veille (Heroku **Basic**, pas Eco, dont la première requête après une
pause prend plusieurs secondes).

## Stockage clé-valeur

À côté de sa base, chaque application a un **stockage clé-valeur** : un cache devant tout ce
qui est lent, des sessions, des compteurs, des limites de débit, des verrous. Par défaut, il
est dans la mémoire du serveur. Rien ne touche le disque : une lecture ou une écriture coûte un
aller-retour, et le serveur en traite plus de 140 000 par seconde sur 4 cœurs. Un redémarrage
le vide. La mémoire est bornée pour chaque application (`NUAGE_KV_MB`) : au-delà, les clés
expirées partent d'abord, puis les moins récemment utilisées.

**En mémoire ou dans la base.** `NUAGE_KV_STORAGE` dit où sont gardés les stockages de toutes
les applications. `memory`, la valeur par défaut, est le stockage ci-dessus : le plus rapide,
et vidé par un redémarrage. `database` garde les clés de chaque application dans sa propre
base, sur disque. Elles sont écrites par le même journal que ses lignes : elles survivent à un
redémarrage, sont chiffrées et sont dans les sauvegardes. Les requêtes, les commandes et le
port restent les mêmes. Avec `database` :

- **une écriture attend le disque**, comme la mise à jour d'une ligne : elle reçoit sa réponse
  une fois synchronisée (les écritures faites en même temps partagent une synchronisation).
  Une lecture n'attend pas ;
- **rien n'est évincé**, et `NUAGE_KV_MB` ne sert pas : le stockage est borné par le disque,
  comme les tables ; donnez donc une expiration aux entrées de cache. Une valeur fait 64 Mio
  au plus ;
- **une liste de commandes, ou `MULTI` … `EXEC`, est un seul commit** : tout ou rien. Une
  écriture que le disque refuse est une erreur (`500` en HTTP, `-ERR storage: …` sur le port),
  jamais à moitié écrite ;
- **une expiration est une date** : une clé faite pour vivre une heure expire une heure plus
  tard, que le serveur ait redémarré entre-temps ou non. Une clé expirée n'est jamais rendue
  ni comptée, et des passages en arrière-plan l'effacent du disque, l'un après l'autre tant
  que des clés sont échues, aussi vite que les clés expirent ;
- **`SCAN` répond page par page** : `COUNT` clés du stockage au plus par page (10 sans lui),
  et un curseur pour la page suivante, `0` après la dernière, comme Redis : `scan_iter` et ses
  pareils parcourent n'importe quel nombre de clés, même en les supprimant au passage. Un
  curseur est gardé 10 minutes au moins, quoi que les autres clients parcourent entre-temps ;
  celui donné avant un redémarrage est refusé (`ERR invalid cursor` : reprenez à `SCAN 0`),
  jamais pris pour une autre page. `KEYS` répond 100 000 clés au plus (16 Mio de clés) :
  au-delà, il dit d'utiliser `SCAN` ;
- **sur un disque effacé à chaque redémarrage** (Heroku), un redémarrage propre garde toutes
  les clés : au `SIGTERM`, le serveur cesse de prendre les écritures, puis envoie son dernier
  segment de journal, qui contient donc chaque écriture à laquelle il a répondu (une qui
  arrive après reçoit l'erreur de stockage). Après un crash,
  les clés reviennent telles qu'au dernier segment envoyé, comme les lignes (30 secondes par
  défaut) : un verrou libéré dans cet intervalle est de nouveau pris, un compteur augmenté
  revient à sa valeur d'avant ;
- les chiffres (`GET /v1/kv`, `/api/status`) disent `"storage": "database"`, avec `max_bytes`
  à 0 ; `INFO` dit `maxmemory:0` et `maxmemory_policy:noeviction`. Les branches et le SQL ne
  voient pas les clés.

Pour en changer, modifiez la ligne dans le fichier de réglages et redémarrez le serveur (voir
la ligne « redémarrer » d'[Installer sur n'importe quel
ordinateur](#installer-sur-nimporte-quel-ordinateur)) ; sur Heroku, `heroku config:set
NUAGE_KV_STORAGE=database`, qui redémarre le dyno. Le journal du démarrage dit `key-value
stores are kept in each application's database`. Passer de `memory` à `database` commence avec
ce que la base contient déjà, rien la première fois : les clés en mémoire ne sont pas copiées.
Revenir à `memory` laisse les clés dans la base, sans les lire, et elles sont retrouvées quand
`database` revient. Toute autre valeur arrête le serveur au démarrage.

**En HTTP**, depuis n'importe quel langage, avec un jeton de l'application (un jeton en lecture
seule lit) :

| Requête | Quoi |
|---|---|
| `GET /v1/kv/<clé>` | la valeur, telle qu'elle a été écrite (`404` s'il n'y en a pas) ; l'en-tête `nuage-ttl-ms` dit combien de temps elle vit encore |
| `PUT /v1/kv/<clé>` (`?ttl=60`, `&nx`, `&xx`, `&keepttl`) | écrit le corps sous la clé : `{"set": true}` ; `nx` n'écrit qu'une clé qui n'existe pas (un verrou) |
| `DELETE /v1/kv/<clé>` | la supprime : `{"deleted": 1}` |
| `POST /v1/kv/<clé>?incr` (`=5`, `&ttl=60`) | ajoute à un compteur : `{"value": 6}` ; `ttl` est l'expiration d'un compteur que cet appel crée (une limite de débit en une requête) |
| `POST /v1/kv/<clé>?expire=60`, `?persist` | change son expiration |
| `GET /v1/kv` (`?prefix=`, `&limit=`) | les clés (taille, temps restant) et les chiffres du stockage |
| `POST /v1/kv` | des commandes en JSON : `["SET", "k", "v", "EX", 60]` répond `{"result": "OK"}` ; une liste de commandes répond `{"results": [...]}` en un aller-retour |
| `DELETE /v1/kv` | supprime toutes les clés |

```bash
curl -X PUT "https://mon-nuage.herokuapp.com/v1/kv/session:42?ttl=1800" -H "Authorization: Bearer $NUAGE_TOKEN" -d 'ada'
curl "https://mon-nuage.herokuapp.com/v1/kv/session:42" -H "Authorization: Bearer $NUAGE_TOKEN"        # ada
curl -X POST "https://mon-nuage.herokuapp.com/v1/kv/visites?incr&ttl=60" -H "Authorization: Bearer $NUAGE_TOKEN"   # {"value":1}
curl https://mon-nuage.herokuapp.com/v1/kv -H "Authorization: Bearer $NUAGE_TOKEN" -d '[["GET","session:42"],["TTL","session:42"]]'
```

**Sur un port à lui** : avec `NUAGE_KV_PORT=6379`, le stockage répond aussi sur une connexion
qui reste ouverte, un jeton de l'application comme mot de passe. Son protocole est RESP (2 et
3), celui que parlent les clients Redis : aucun client à écrire, `redis-cli`, redis-py,
node-redis, go-redis, Jedis… se connectent tels quels :

```bash
redis-cli -u redis://default:<jeton>@db.example.com:6379 SET salut bonjour EX 60
```

Textes et compteurs : `GET`, `SET` (`EX`, `PX`, `NX`, `XX`, `KEEPTTL`, `GET`), `SETNX`, `SETEX`,
`GETSET`, `GETDEL`, `GETEX`, `MGET`, `MSET`, `APPEND`, `STRLEN`, `INCR`, `DECR`, `INCRBY`,
`DECRBY`, `DEL`, `EXISTS`, `EXPIRE`, `PEXPIRE`, `TTL`, `PTTL`, `PERSIST`, `TYPE`, `KEYS`, `SCAN`,
`DBSIZE`, `FLUSHDB`, `INFO`, `PING`, et `MULTI` … `EXEC` (les commandes s'exécutent comme une
seule). Les listes, les hachages, les ensembles et pub/sub n'y sont pas. Avec `NUAGE_TLS_CERT`
et `NUAGE_TLS_KEY`, le même port répond aussi en TLS. Heroku ne route que HTTP :
là, utilisez `/v1/kv`.

**Depuis Rust**, `NuageKv` de Memoire (`db.kv()`) : `get`, `set_for`, `incr_by`,
`set_if_absent`, et `remember(clé, durée, || calcul())`, un cache en un appel. En embarqué,
`nuage::kv::Kv` est le stockage lui-même.

## Fichiers

À côté de sa base, une application peut garder des **fichiers** (des tuiles de carte, des
photos, des archives de plusieurs gigaoctets) dans un bucket compatible S3 : Cloudflare R2, AWS
S3, MinIO, Scaleway… Le serveur tient la liste et signe les liens ; **les octets vont
directement du client au bucket**, jamais par le serveur : un petit serveur (un dyno Heroku)
distribue des téraoctets. Chaque application a son dossier, `<préfixe><application>/` : son
jeton ne voit que ses fichiers.

| Requête | Quoi | Jeton |
|---|---|---|
| `GET /v1/files` (`?prefix=regions/`) | les fichiers : `{"files": [{"name", "size", "updated", "etag"}], "count", "bytes"}` | lecture seule ou lecture-écriture |
| `GET /v1/files/<nom>` | `307` vers un lien de téléchargement signé (`Range` marche : un téléchargement reprend où il s'est arrêté) ; `?link` : le lien en JSON, `{"url", "expires_at"}` ; `?download=<nom de fichier>` : enregistré sous ce nom | lecture seule ou lecture-écriture |
| `POST /v1/files/<nom>` | un lien d'envoi signé, `{"method": "PUT", "url", "expires_at"}` : y envoyer les octets en `PUT` (5 Go au plus chacun) | lecture-écriture |
| `DELETE /v1/files/<nom>` | supprime le fichier | lecture-écriture |

Un nom, ce sont des lettres, des chiffres, `.`, `_` et `-`, en parties séparées par `/`
(`regions/fr.tar`). Un lien marche une heure (`NUAGE_FILES_LINK_MINUTES`, 7 jours au plus) ;
en demander un nouveau ensuite.
Le bucket refuse un lien accompagné d'un en-tête `Authorization` : curl et les navigateurs le
retirent en suivant la redirection ; un client qui le garde (certaines bibliothèques HTTP) prend
`?link`, puis télécharge l'`url` sans le jeton.

```bash
# Envoi : un lien, puis les octets vers le bucket
url=$(curl -s -X POST https://mon-nuage.herokuapp.com/v1/files/regions/fr.tar \
  -H "Authorization: Bearer $NUAGE_TOKEN" | jq -r .url)
curl -T fr.tar "$url"
# Téléchargement, qui reprend (-C -), avec un jeton en lecture seule
curl -L -C - -o fr.tar https://mon-nuage.herokuapp.com/v1/files/regions/fr.tar -H "Authorization: Bearer $READ_TOKEN"
```

Sur Cloudflare R2 (aucun frais pour les octets téléchargés) : **R2 → Create bucket**, puis
**Manage API tokens → Create API token**, *Object Read & Write* sur ce bucket. Sur le serveur :

```bash
heroku config:set NUAGE_FILES_S3_BUCKET=mes-fichiers \
  NUAGE_FILES_S3_ENDPOINT=https://<id du compte>.r2.cloudflarestorage.com \
  NUAGE_FILES_S3_ACCESS_KEY_ID=... NUAGE_FILES_S3_SECRET_ACCESS_KEY=...
```

## Installer sur n'importe quel ordinateur

Nuage DB tourne sur n'importe quel ordinateur : un serveur Linux, un Raspberry Pi, un Mac, un
PC sous Windows, à côté (ou à la place) d'un serveur chez un hébergeur comme Heroku. Une
commande l'installe comme un service qui démarre avec l'ordinateur, et demande où vont les
sauvegardes.

| | Une commande | Service |
|---|---|---|
| **Linux** (Debian, Ubuntu, Fedora, Rocky, Alpine, Raspberry Pi OS..., x86_64 ou ARM64, dès 512 Mio) | `curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh \| sudo bash` | systemd `nuage` |
| **macOS** (Apple silicon ou Intel), dans le Terminal | `curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh \| sudo bash` | launchd, sous votre utilisateur |
| **Windows** 10, 11, Server, dans PowerShell **en administrateur** | `irm https://raw.githubusercontent.com/maxlestage/Nuage/master/install.ps1 \| iex` | tâche planifiée `Nuage DB`, en SYSTEM |
| **cargo**, partout où tourne Rust | `cargo install --locked --git https://github.com/maxlestage/Nuage nuage` (voir [Avec cargo](#avec-cargo)) | aucun : donnez-le à l'installateur |
| **Docker**, partout | `docker compose up -d` dans ce dépôt (réglages dans `.env`) | le conteneur |

Chacune installe le binaire `nuage` (compilé en statique : aucune dépendance), crée le
fichier de réglages avec une clé, un jeton, un mot de passe de console et un jeton de gestion,
crée le dossier des données, règle les sauvegardes, démarre le service et vérifie qu'il
répond. Elle affiche une fois l'adresse, le jeton et le mot de passe : **copiez le fichier de
réglages en lieu sûr, loin de cet ordinateur** : personne ne peut lire les données, ni leurs
sauvegardes, sans sa clé.

| | Linux | macOS | Windows |
|---|---|---|---|
| réglages | `/etc/nuage/nuage.env` | `/usr/local/etc/nuage/nuage.env` | `C:\ProgramData\Nuage\nuage.env` |
| données | `/var/lib/nuage` | `/usr/local/var/nuage` | `C:\ProgramData\Nuage\data` |
| journal | `journalctl -u nuage -f` | `tail -f /usr/local/var/log/nuage.log` | `Get-Content -Wait C:\ProgramData\Nuage\nuage.log` |
| redémarrer | `sudo systemctl restart nuage` | `sudo launchctl kickstart -k system/io.github.maxlestage.nuage` | `Stop-ScheduledTask -TaskName 'Nuage DB'; Get-Process nuage -ErrorAction SilentlyContinue \| Stop-Process -Force; Start-ScheduledTask -TaskName 'Nuage DB'` |

Une modification du fichier de réglages s'applique quand le service redémarre (avec Docker :
`docker compose up -d` après une modification de `.env`). Sur Windows, l'arrêt est forcé : ce
qui a été écrit est rejoué depuis le journal au démarrage suivant.

Sur Linux, elle **crée aussi du swap** si le serveur n'en a pas (la taille de la mémoire, de
1 à 4 Gio ; `vm.swappiness=10`, pour que la mémoire aille à la base) et un utilisateur système
`nuage`. Sur macOS, le service tourne sous la personne qui a lancé `sudo` ; sur Windows, en
SYSTEM, lisible des seuls administrateurs, avec une règle de pare-feu pour le réseau local.

- **HTTPS** (Linux) : faites pointer un nom de domaine vers le serveur, puis
  `curl -fsSL .../install.sh | sudo NUAGE_DOMAIN=db.example.com bash` (Caddy, avec un
  certificat gratuit ; Nuage DB n'écoute alors que sur `127.0.0.1`). Sans HTTPS, la console
  fonctionne, mais prévient que le mot de passe circule en clair.
- **Mise à jour** : relancez la commande ; données, clé, réglages et sauvegardes restent.
- **Options** : `NUAGE_PORT`, `NUAGE_PG_PORT` (`0` : coupé), `NUAGE_SWAP_MB` (`0` : pas de
  swap), `NUAGE_VERSION=v0.4.11`, `NUAGE_BINARY=/chemin/vers/nuage`, `NUAGE_FROM_SOURCE=1`
  (compiler avec cargo). Sur Windows, définissez-les avant : `$env:NUAGE_PORT = '9090'; irm ... | iex`.
- **Désinstaller** : `... | sudo bash -s uninstall` garde les données, les réglages et les
  sauvegardes ; `... | sudo bash -s purge` efface les données et les réglages, jamais les
  sauvegardes. Sur Windows : `$env:NUAGE_ACTION = 'uninstall'` (ou `'purge'`), puis la même
  commande.
- **Mémoire** : Nuage DB utilise la mémoire du serveur où il tourne, quelle que soit sa
  taille, et pas plus. Les données restent sur disque ; la mémoire va aux caches de pages, qui
  partagent un budget (`NUAGE_CACHE_MB=auto`) qui suit la mémoire : un quart au départ,
  davantage tant qu'il y a de la place et que les caches sont pleins (jusqu'à six dixièmes),
  moins quand la mémoire se tend. Chaque application a ce qu'elle occupe plus une part égale de
  ce qui est libre : le budget va aux applications qui s'en servent. Au-delà de 85 % utilisés,
  les caches sont vidés, leur budget divisé par deux et la mémoire libérée rendue au système,
  pendant que les requêtes continuent. La page *Serveur* de la console et la carte **Mémoire**
  du tableau de bord montrent la mémoire dont ce serveur dispose (la limite de son conteneur,
  s'il en a une : chez un hébergeur mutualisé, pas la mémoire de toute la machine), ce qui est
  utilisé et ce qui reste, en gigaoctets.
- **À la main** : des binaires pour Linux, macOS et Windows sont sur la
  [page des versions](https://github.com/maxlestage/Nuage/releases) ; `nuage --env-file nuage.env`
  démarre le serveur avec les réglages d'un fichier (lignes `CLÉ=valeur` ; l'environnement
  l'emporte).

### Avec cargo

Avec Rust installé ([rustup.rs](https://rustup.rs)), sur Linux, macOS ou Windows :

```bash
cargo install --locked --git https://github.com/maxlestage/Nuage nuage
nuage --version
```

Le binaire `nuage` arrive dans `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` sur Windows), avec
le site et le tableau de bord en direct. Un serveur plus léger, sans eux (`/` est alors une
petite page d'accueil ; la console, l'API et tout le reste demeurent) :
`cargo install --locked --git https://github.com/maxlestage/Nuage nuage --no-default-features --features server`.
Une version précise : `--tag v0.4.11`. Mise à jour : la même commande, avec `--force`.

`cargo install` ne pose que le binaire : ni service, ni réglages, ni sauvegardes. Donnez-le à
l'installateur, qui fait le reste (réglages, sauvegardes, service lancé avec l'ordinateur) :

```bash
# Linux, macOS : `command -v nuage` le trouve avant sudo, dont le PATH n'a pas ~/.cargo/bin
curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh | sudo NUAGE_BINARY="$(command -v nuage)" bash
```

```powershell
# Windows, PowerShell en administrateur
$env:NUAGE_BINARY = (Get-Command nuage).Source; irm https://raw.githubusercontent.com/maxlestage/Nuage/master/install.ps1 | iex
```

Ou lancez-le vous-même, avec un fichier de réglages : `nuage genkey` crée la clé,
`nuage --env-file nuage.env` démarre le serveur (voir [`nuage.env.example`](nuage.env.example)).
Sur Linux et macOS, `NUAGE_FROM_SOURCE=1` fait compiler l'installateur lui-même avec cargo.

### Sauvegardes : vous choisissez où

Le dossier des données n'est qu'une copie. Avec une sauvegarde, Nuage DB envoie un
**instantané** de chaque base (toutes les heures, `NUAGE_BACKUP_SNAPSHOT_MINUTES`) et le
**journal d'écriture** depuis (à chaque checkpoint, toutes les 30 secondes), le tout chiffré
avec la clé ; et quand il démarre avec un **dossier de données vide** (un nouveau disque, un
nouvel ordinateur), il **se restaure tout seul** depuis la sauvegarde avant de servir.
L'installateur demande où elles vont ; ou dites-le dans la commande :

| Où | Commande (Linux, macOS) | Windows |
|---|---|---|
| un dossier sur **un autre disque** : un disque externe, un NAS | `curl ... \| sudo NUAGE_BACKUP_DIR=/Volumes/MonDisque/nuage bash` | `$env:NUAGE_BACKUP_DIR = 'E:\Nuage'; irm ... \| iex` |
| un **stockage compatible S3** : AWS S3, Cloudflare R2, Scaleway, Backblaze B2, MinIO | `NUAGE_BACKUP_S3_BUCKET=… NUAGE_BACKUP_S3_REGION=… NUAGE_BACKUP_S3_ENDPOINT=… NUAGE_BACKUP_S3_ACCESS_KEY_ID=… NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=…` | les mêmes, avec `$env:` |
| aucune (déconseillé) | `NUAGE_BACKUP=none` | `$env:NUAGE_BACKUP = 'none'` |

- **Un autre disque, ou ailleurs.** Une sauvegarde sur le disque des données meurt avec lui :
  l'installateur et le serveur préviennent quand le dossier de sauvegarde est sur le même
  disque. Une copie à la maison ne survit pas à un incendie ni à un vol : un stockage
  compatible S3, si (R2 et B2 coûtent peu pour cette taille). Gardez aussi la clé du fichier
  de réglages loin du bucket des sauvegardes.
- **Un disque externe débranché** : l'installateur ne crée que le dernier dossier de
  `NUAGE_BACKUP_DIR` (`/Volumes/MonDisque` doit exister), et le serveur prévient au démarrage
  quand le dossier se trouve en fait sur le disque de l'ordinateur. Sur macOS, si les
  sauvegardes vers un disque externe échouent (dans le journal), ajoutez
  `/usr/local/bin/nuage` dans Réglages Système → Confidentialité et sécurité → Accès complet
  au disque. Sur Windows, un partage de NAS (`\\nas\sauvegarde\nuage`) doit laisser écrire le
  compte de l'ordinateur (`ORDINATEUR$`).
- **Changer** de destination, sur un disque qui n'est pas effacé (votre ordinateur, un
  serveur) : relancez l'installateur avec la nouvelle option. Il remplace l'ancienne dans le
  fichier de réglages et redémarre le service ; sans option, il les garde. Pour un bucket,
  modifier le fichier de réglages et redémarrer le service revient au même. Sur Linux, un
  nouveau dossier passe par l'installateur, qui permet aussi au service d'y écrire. Juste
  après le démarrage, le serveur envoie un instantané complet de chaque application au nouvel
  endroit. L'ancien reste tel quel : effacez-le une fois que la console montre les sauvegardes
  du nouveau.
- **Une base n'écrit jamais par-dessus la sauvegarde d'une autre.** Avant chaque instantané,
  le serveur compare le fichier `encryption.json` de la destination au sien. Un endroit qui
  contient la sauvegarde d'une autre base est refusé, et rien n'y est écrit. Le journal
  d'événements dit `backup failed: … holds the backup of another database (its encryption.json
  is not this one's): nothing was written there`, et la console comme `/app` montrent la
  sauvegarde en échec. Donnez à cette base un endroit à elle, ou videz celui-là. Un bucket qui
  n'existe pas est une erreur aussi, jamais une sauvegarde vide.
- **Un démarrage à vide est dit, et peut être refusé.** Quand le dossier des données est vide
  et que la sauvegarde ne contient rien à restaurer, le serveur démarre une base neuve et vide,
  et le dit dans son journal (`warning: the data directory was empty and … holds no backup:
  this is a NEW, EMPTY database`) et dans le journal d'événements ; une application dont la
  sauvegarde ne contient rien le dit aussi. Avec `NUAGE_RESTORE_REQUIRED=true`, ce démarrage
  est refusé à la place : le serveur s'arrête et dit quoi vérifier, sans rien avoir créé ni
  écrit. Une application enregistrée dont la sauvegarde ne contient rien n'est pas servie non
  plus : le serveur démarre sans elle, et la console la range parmi celles qui ne s'ouvrent
  pas (complétez sa sauvegarde et redémarrez, ou supprimez-la là). Activez-le une fois que le
  serveur contient des données : un premier démarrage n'a rien à restaurer.
- **Deux endroits à la fois** (expérimental) : `NUAGE_BACKUP2_DIR`, ou `NUAGE_BACKUP2_S3_BUCKET` et les autres
  `NUAGE_BACKUP2_S3_*`, ajoutent une seconde destination tenue à jour avec la première.
  Chacune reçoit les instantanés et le journal à son propre rythme : celle qui ne répond pas
  ne retient jamais l'autre (chacune a son propre fil), et reçoit un instantané complet quand
  elle répond de nouveau. Un dossier de données vide est restauré depuis une seule des deux, en
  entier : celle qui va le plus loin. Le serveur ne démarre pas quand l'une des deux ne répond
  pas, puisque ce peut être celle qui va le plus loin (retirez-la des réglages pour démarrer
  depuis l'autre seule ; une seconde qui n'a pas répondu 5 secondes après la première compte
  comme ne répondant pas), ni quand les deux tiennent des bases différentes. Sans la première,
  `NUAGE_BACKUP2_*` est l'unique destination. C'est encore expérimental : ce n'est pas encore
  une façon sûre de déplacer les sauvegardes ailleurs (déplacez-les avec un arrêt, comme
  indiqué pour Heroku), et ses limites connues sont listées dans le CHANGELOG. La page *Stockage* de la console montre la dernière sauvegarde de chaque
  destination, et `/api/status` chacune dans `backup.destinations` : une destination
  n'affiche pas d'heure tant qu'elle n'a pas reçu un instantané complet depuis le démarrage,
  et la dernière sauvegarde n'en affiche pas tant que les deux n'en ont pas reçu. Un tour vers
  une destination qui n'est pas fini au bout d'une heure (elle ne répond pas, ou si
  lentement qu'elle ne suit jamais) s'affiche comme son erreur, et dans le journal des
  événements, jusqu'à ce qu'il finisse. L'arrêt envoie aux deux en même temps : la première a
  le temps qu'il faut, la seconde 20 secondes au plus. Sur Linux, le service de l'installateur ne peut écrire que dans le dossier des
  données et dans `NUAGE_BACKUP_DIR` : là, prenez un bucket pour la seconde.
- **Supprimer une application** dans la console vide sa sauvegarde (`apps/<nom>/`) dans chaque
  destination, une fois fini le tour de sauvegarde en cours vers elle, s'il y en a un : rien
  d'elle n'y est écrit ensuite. Celle qui ne peut pas être vidée est nommée dans la réponse et
  le journal des événements ; tant que sa sauvegarde n'y est pas supprimée à la main, aucune
  application de ce nom ne peut être créée, pour qu'une nouvelle ne démarre jamais avec les
  lignes de la supprimée. Celle qui n'a pas répondu en 10 secondes est nommée aussi, et son
  effacement continue : le nom reste pris jusqu'à ce qu'il finisse.
- **Vérifier** : la page *Stockage* de la console, et `/app`, montrent la dernière sauvegarde
  et ses erreurs. Un bucket qui ne répond plus fait échouer le tour au bout de 60 secondes
  sans que rien ne bouge, et le tour suivant réessaie ; un envoi n'est jamais coupé tant que
  ses octets continuent de quitter le serveur (la limite compte depuis le dernier octet parti
  sur la ligne, pas depuis ce qui attend de partir).
- **Restaurer** sur un nouvel ordinateur : installez, mettez dans le nouveau fichier de
  réglages la `NUAGE_ENCRYPTION_KEY` de l'ancien et les mêmes réglages de sauvegarde, videz le
  dossier des données et redémarrez le service : il restaure le dernier instantané et rejoue
  le journal qui le suit.
- **Un seul écrivain par sauvegarde** : deux serveurs ne doivent jamais sauvegarder dans le
  même dossier ni sous le même préfixe de bucket. Un serveur qui a une autre base y est refusé
  (voir plus haut), mais deux copies d'une même base, comme deux dynos restaurés depuis la même
  sauvegarde, s'écraseraient l'une l'autre. Un ordinateur à côté d'un serveur Heroku a son
  propre dossier, son bucket ou son `NUAGE_BACKUP_S3_PREFIX`.

#### Une copie de secours, choisie plus tard

Contrairement à `NUAGE_BACKUP2_*` (plus haut, réglée dans l'environnement et tenue à jour au fil
du journal), cette copie se choisit depuis la console, pendant que le serveur tourne, et c'est une
copie complète toutes les heures.

En plus de la destination nommée par l'environnement, la page Réglages de la console (section
*Sauvegardes*), `nuage backup second dir /Volumes/AutreDisque/nuage` (ou `nuage backup second s3
<bucket> [endpoint] [region] [prefix]`, les clés dans `NUAGE_SECOND_S3_ACCESS_KEY_ID` et
`NUAGE_SECOND_S3_SECRET_ACCESS_KEY`) et `POST /api/backup/second` choisissent une **copie de
secours** : un dossier sur un autre disque, ou un stockage compatible S3. C'est la seule quand
l'environnement n'en nomme aucune (sur un ordinateur dont le disque reste).

- C'est une **copie complète de chaque base**, écrite quand elle est enregistrée, au démarrage
  du serveur, puis **toutes les heures** : jusqu'à une heure d'écritures peut y manquer, là où la
  première destination reçoit aussi le journal toutes les 30 secondes.
- Chaque démarrage du serveur écrit dans **son propre dossier** (`lives/<début>-<id>/`) : rien
  n'y est jamais écrasé, quoi que d'autre écrive au même endroit ; les trois plus récentes
  complètes sont gardées.
- L'endroit est vérifié avant d'être enregistré (écrit, relu ; jamais dans le dossier des
  données, ni là où vont déjà les sauvegardes) ; son secret S3 est gardé avec les réglages,
  chiffré avec les données, et n'est plus jamais affiché.
- Le serveur **n'en restaure jamais de lui-même**. `nuage backup`, la page et `GET /api/backup`
  donnent les lignes qui démarrent un serveur depuis la copie d'une exécution : mettez-les dans
  son environnement, avec un dossier de données vide et la même `NUAGE_ENCRYPTION_KEY`.
- Sur un hébergeur dont le disque est effacé à chaque redémarrage (Heroku), une copie de
  secours demande une destination dans l'environnement : enregistrée seule, elle serait perdue
  avec le disque.

## Démarrage rapide

```bash
git clone https://github.com/maxlestage/Nuage && cd Nuage
# La clé qui chiffre tout : gardez-la, sans elle les données sont illisibles.
export NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32)
NUAGE_TOKEN=dev-token NUAGE_ADMIN_PASSWORD=dev-password cargo run -p nuage --release
# nuage: data is stored in /home/moi/Nuage/nuage-data on this server's disk
# nuage: listening on 0.0.0.0:8080
```

Puis ouvrez :

- http://localhost:8080/ : le site (anglais, français, espagnol) ;
- http://localhost:8080/app : le tableau de bord en direct (connexion avec `NUAGE_ADMIN_PASSWORD`) ;
- http://localhost:8080/admin : la console d'administration.

Les applications se connectent avec `NuageDb::connect("http://localhost:8080", "dev-token")`.

Docker (Compose, avec les réglages dans `.env`, d'après [`nuage.env.example`](nuage.env.example)) :

```bash
cp nuage.env.example .env   # puis renseignez NUAGE_ENCRYPTION_KEY, NUAGE_TOKEN, NUAGE_ADMIN_PASSWORD
docker compose up -d
```

## Déployer sur Heroku

La racine de ce dépôt se déploie telle quelle : `Procfile`, `RustConfig` et
`heroku-build.sh` (le buildpack `emk/rust` compile le binaire `nuage` avec le site), et
`app.json` pour le bouton **Deploy to Heroku** ci-dessus.

- **Un dyno Basic.** Un dyno Basic ne se met jamais en veille. Lancez exactement un dyno
  web : deux dynos feraient deux bases distinctes qui écrivent dans les mêmes sauvegardes.
- **Le disque est éphémère.** Le disque d'un dyno est effacé à chaque redémarrage (au moins
  une fois par jour, et à chaque déploiement). Le répertoire de données n'est qu'une copie de
  travail : **configurez les sauvegardes**, et chaque démarrage restaure la base à partir
  d'elles. Sur Heroku, utilisez un stockage compatible S3 (`NUAGE_BACKUP_S3_*`) ; chez un
  hébergeur au disque persistant, un dossier suffit (`NUAGE_BACKUP_DIR`).
- **Fenêtre de perte.** Les commits faits depuis le dernier segment de journal envoyé
  (30 secondes par défaut) ne sont perdus que si le dyno s'arrête sans le `SIGTERM` qu'Heroku
  envoie d'abord : au `SIGTERM`, le serveur cesse de prendre les écritures (une qui arrive
  après reçoit une erreur, jamais une réponse que la sauvegarde n'a pas), fait un checkpoint
  et envoie le dernier segment.
  Les clés des stockages clé-valeur suivent la même règle avec `NUAGE_KV_STORAGE=database` ;
  en mémoire, un redémarrage les vide.
- **Changer un réglage redémarre le dyno.** Chaque `heroku config:set` ou `config:unset` le
  redémarre : son disque est effacé et la base est restaurée depuis la sauvegarde que nomment
  les nouveaux réglages. Ne faites donc jamais pointer les variables de sauvegarde, en une
  commande, vers un endroit qui ne contient pas la sauvegarde : le dyno n'y trouverait rien et
  démarrerait une base neuve et vide (son journal le dit, et l'ancien endroit garde ses
  sauvegardes). Déplacez-les comme ci-dessous.
- **Refuser un démarrage à vide.** Une fois que le serveur contient des données, faites
  `heroku config:set NUAGE_RESTORE_REQUIRED=true`. Dès lors, un dyno qui ne trouve rien à
  restaurer (un bucket ou un préfixe mal écrit, les variables de sauvegarde retirées) ne
  démarre pas, et ne crée ni n'écrit rien. Heroku le montre planté, `heroku logs --tail` dit
  quoi vérifier, et remettre les réglages le redémarre depuis sa sauvegarde. Pas lors d'un
  premier déploiement, qui n'a rien à restaurer : `app.json` ne le définit pas.
- **Déplacer les sauvegardes avec un arrêt.** Changer les variables de sauvegarde redémarre
  le dyno sur le nouvel endroit : arrêtez-le d'abord, copiez-y la sauvegarde avec les outils
  du stockage, puis changez les réglages et redémarrez-le (les commandes sont plus bas). La
  seconde destination (`NUAGE_BACKUP2_*`) est encore expérimentale, et pas encore une façon
  sûre de les déplacer.

À la main :

```bash
heroku create mon-nuage --buildpack emk/rust
heroku config:set NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32) NUAGE_TOKEN=$(openssl rand -hex 32) \
  NUAGE_ADMIN_PASSWORD='une longue phrase de passe' \
  NUAGE_BACKUP_S3_BUCKET=mon-bucket NUAGE_BACKUP_S3_REGION=eu-west-3 \
  NUAGE_BACKUP_S3_ACCESS_KEY_ID=... NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=...
git push heroku master
```

Pour un stockage autre qu'AWS, définissez aussi `NUAGE_BACKUP_S3_ENDPOINT`. Ouvrez ensuite
`https://mon-nuage.herokuapp.com/admin` : sa page *Stockage* (ou `backup.last_ok` dans
`/api/status`) doit montrer la première sauvegarde, sans erreur.

Pour déplacer les sauvegardes vers un autre bucket :

```bash
heroku config:set NUAGE_RESTORE_REQUIRED=true        # une fois : désormais un démarrage à vide est refusé
heroku ps:scale web=0                                # le serveur envoie sa dernière sauvegarde et s'arrête
aws s3 sync s3://ancien-bucket/nuage/ s3://nouveau-bucket/nuage/   # tous les objets (ou rclone sync)
heroku config:set NUAGE_BACKUP_S3_BUCKET=nouveau-bucket NUAGE_BACKUP_S3_REGION=… \
  NUAGE_BACKUP_S3_ACCESS_KEY_ID=… NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=…   # toutes, en une seule commande
heroku ps:scale web=1                                # il restaure depuis le nouveau bucket, ou ne démarre pas
```

## Réglages : à vous de choisir, et de changer d'avis

Celui qui fait tourner un serveur Nuage DB, sur son ordinateur ou en ligne, choisit comment
il fonctionne et le change plus tard, sans redémarrage :

- **la page Réglages de la console** (`/admin/settings`), en français, anglais et espagnol ;
- **`nuage settings`** dans un terminal : `nuage settings` les liste, `nuage settings set
  memory.cache_mb 2G` en change un, `nuage settings unset memory.cache_mb` le rend ;
- **`/api/settings`**, avec le jeton de gestion (`GET` ; `POST {"memory.cache_mb": 2048}` ;
  `DELETE /api/settings/memory.cache_mb`).

Ce qui est enregistré est **en vigueur tout de suite** et **conservé avec les données** :
chiffré, sauvegardé et restauré avec elles. Cela l'emporte sur l'environnement (le fichier de
réglages, les variables de l'hébergeur), qui l'emporte sur la valeur par défaut ; chaque
réglage dit d'où vient sa valeur, et un clic la rend à l'environnement. Avec
`NUAGE_SETTINGS=environment`, l'environnement est la seule source : ce qui avait été
enregistré est ignoré (la page le montre, et le retire si vous le demandez).

| Réglage | Quoi | Valeur de départ |
|---|---|---|
| `memory.total_mb` | la mémoire sur laquelle Nuage DB peut compter : `auto` (la limite du conteneur, sinon toute la mémoire de la machine : Linux, macOS, Windows), ou une quantité qui laisse le reste aux autres programmes | `NUAGE_MEMORY_MB` |
| `memory.cache_mb` | les caches de pages de toutes les applications ensemble : `auto` (il suit la mémoire) ou un maximum | `NUAGE_CACHE_MB` |
| `memory.kv_mb` | le stockage clé-valeur de chaque application : `auto` ou une quantité | `NUAGE_KV_MB` |
| `statement.memory_mb` | ce qu'une instruction SQL peut garder en mémoire à la fois (`auto` : un huitième de la mémoire) | `NUAGE_STATEMENT_MB` |
| `statement.seconds` | combien de temps une instruction peut durer (`0` : pas de limite) | `NUAGE_STATEMENT_SECONDS` |
| `postgres.connections` | connexions PostgreSQL servies à la fois | `NUAGE_PG_MAX_CONNECTIONS` |
| `status.public` | l'état public (`true`, `false`) | `NUAGE_PUBLIC_STATUS` |

Une quantité s'écrit `auto`, `512 Mio` ou `2 Gio` (`2G`, `2 Go`, `1,5 Go`) ; un nombre seul
est en Mio.

**La mémoire : celle du serveur, ou par base de données.** Par défaut, un serveur utilise la
mémoire qu'il a : un seul budget pour les caches de pages de toutes ses applications, qui
suit la mémoire et va aux applications qui s'en servent. Une application (une base de
données) peut à la place avoir **sa propre part**, réservée : dans son onglet Réglages, dans
le tableau *Mémoire par application* de la page Réglages, avec `nuage apps memory shop 2G`
(puis `auto` pour partager de nouveau), ou `POST /api/apps/shop/memory {"cache_mb": 2048,
"kv_mb": "auto"}`. Son cache dispose alors de cette quantité quoi que fassent les autres, qui
se partagent le reste. Il en va de même pour son stockage clé-valeur.

Nuage DB vérifie ce qui est fixé à la main avant de l'enregistrer, et donne les chiffres
quand il refuse : les stockages clé-valeur dans un quart de la mémoire, et avec les caches
de pages fixés à la main dans trois quarts ; une mémoire dite à la main ne peut que baisser
celle que le système annonce. Quand la mémoire manque, le garde-fou garde le dernier mot :
ce qui est réservé diminue avec le reste, puis revient.

**Ce qui reste dans l'environnement.** Ce que le serveur lit avant de pouvoir ouvrir ses
données (la clé de chiffrement, le dossier des données), ce sur quoi il écoute (ports,
adresses, certificats), ce qui y donne accès (mot de passe, jetons) et la destination de ses
sauvegardes se lisent une fois, au démarrage : une mauvaise valeur enregistrée depuis la
console pourrait vous en fermer la porte. La page Réglages les liste (un secret s'affiche
comme réglé, jamais sa valeur) et dit comment les changer sur cet hébergement.

### Dans une fenêtre : Nuage DB Éditeur

Pour celles et ceux qui préfèrent éviter le terminal, **Nuage DB Éditeur** est un programme
avec une fenêtre, en français, en anglais et en espagnol (la langue du système, ou celle que
vous choisissez), clair, sombre ou comme le système, aux couleurs de la console. Il s'occupe
d'un serveur par son API de gestion, et ne change rien de lui-même : chaque changement est un
bouton que vous pressez, et il dit ce qu'il a fait.

- **Voir qu'il fonctionne** : sa version, depuis quand il tourne, la mémoire qu'il occupe et
  celle de l'ordinateur, ses applications et leur dernière connexion, et si les sauvegardes
  fonctionnent (la dernière copie, quand, vers où, et en clair l'erreur quand l'une a échoué).
- **Créer une application** pour un site ou un programme : dites à quoi elle sert, son nom
  suit. Ses deux lignes (`NUAGE_URL`, `NUAGE_TOKEN`) sont montrées une fois, masquées tant que
  vous ne les affichez pas, et copiées en priant l'historique du presse-papiers de les ignorer.
  **Tester le jeton** appelle le serveur avec lui depuis cet ordinateur ; la page de
  l'application dit ensuite qu'il a été testé depuis l'éditeur, et quand votre site s'est
  connecté pour la dernière fois tel que l'éditeur l'a vu (le serveur ne garde que la dernière
  connexion).
- **Brancher une page web** : un jeton en lecture seule, le site à autoriser
  (`https://monsite.fr`), et le script à coller dans la page. Un jeton se révoque après une
  question.
- **Mémoire et limites** : en vigueur tout de suite, sans redémarrage ; un refus arrive dans
  votre langue, avec ses chiffres. Ce que le serveur lit au démarrage (ports, clés, destination
  des sauvegardes) est affiché, jamais changé, avec l'endroit où cela se change sur cet
  hébergement.
- **Outils** : tester le jeton d'une application depuis n'importe où, et générer un jeton
  (comme `nuage genkey`).

Supprimer une application reste dans la console, derrière sa confirmation tapée. Un serveur de
0.4.2 à 0.4.6 s'ouvre aussi, pour ses applications et ses jetons ; pour ce qui lui manque (les
réglages, l'état, les sites web), l'éditeur dit qu'une mise à jour du serveur le montrerait.

1. **Téléchargez-le** depuis la [dernière version](https://github.com/maxlestage/Nuage/releases/latest) :
   `Nuage-DB-Editor-macOS.dmg` (tout Mac sous macOS 11 ou plus récent, puce Apple ou Intel),
   `Nuage-DB-Editor-windows-x64.exe` (Windows 10 et 11), `nuage-editor-x86_64-linux.tar.gz`
   (un bureau Linux, Ubuntu 20.04, Debian 11 ou plus récent). Chacun a son `.sha256`.
2. **Ouvrez-le.** Il n'est pas signé par un certificat de développeur : la première
   ouverture demande une fois. macOS : ouvrez le `.dmg`, glissez *Nuage DB Editor* dans
   Applications, ouvrez-le ; macOS dit ne pas pouvoir le vérifier : Réglages Système,
   Confidentialité et sécurité, **Ouvrir quand même**, puis votre mot de passe. Windows :
   double-cliquez sur le `.exe` (rien à installer) ; Edge peut dire qu'il est rarement
   téléchargé : **Conserver** ; SmartScreen dit « Windows a protégé votre ordinateur » :
   **Informations complémentaires**, puis **Exécuter quand même**. Linux : décompressez, puis
   clic droit sur `nuage-editor`, **Exécuter comme un programme** (GNOME Fichiers ne lance pas
   un programme d'un double-clic) ; dans l'éditeur, **À propos**, puis **Ajouter l'éditeur au
   menu des applications** le met dans le menu (`README.txt` donne la même chose en commandes).
3. **Connectez-vous.** Un serveur installé sur cet ordinateur par l'installateur est trouvé
   tout seul : **Ouvrir**, avec une fenêtre du système au plus. L'éditeur ne tourne jamais en
   administrateur ; les outils du système lisent pour lui quatre lignes du fichier de
   réglages (`PORT`, `NUAGE_ADMIN_TOKEN`, `SITE_URL`, `NUAGE_BIND` ; jamais la clé de
   chiffrement) : pkexec et grep après votre mot de passe sous Linux, osascript et grep sous
   macOS (sans mot de passe pour le compte qui a installé), cmd.exe et findstr de Windows
   après la fenêtre UAC, qui nomme *Processeur de commandes Windows*, de Microsoft ; l'éditeur
   le dit avant que la fenêtre s'ouvre. Pour un serveur ailleurs, tapez son adresse et son
   jeton de gestion, `NUAGE_ADMIN_TOKEN` : *Où le trouver ?* montre où il est sur Heroku
   (Settings de l'application, **Reveal Config Vars**), sur un serveur posé par l'installateur,
   et dans le `.env` de Docker, et en génère un quand il n'y en a pas. La connexion se fait pas
   à pas (le serveur, le jeton, ses réglages), avec **Ne plus attendre** après 3 secondes ;
   quand une étape échoue, l'éditeur dit laquelle, pourquoi, et quoi essayer. Il retient
   l'adresse ; le jeton seulement si vous cochez *Retenir sur cet ordinateur*, et alors dans le
   gestionnaire de mots de passe de votre système (le trousseau de macOS, le Gestionnaire
   d'identification de Windows, le trousseau d'un bureau Linux), jamais dans un fichier à
   lui. Il n'envoie jamais le jeton après une redirection, et demande avant de l'envoyer en
   clair (`http://`) à une adresse d'Internet.

Ce qu'il change est enregistré par le serveur, comme depuis la console : en vigueur tout de
suite, gardé avec les données. Les réglages lus au démarrage du serveur (la clé de
chiffrement, les ports, les mots de passe, la destination des sauvegardes) sont affichés,
et l'éditeur dit comment les changer sur cet hébergement. Lancé avec `NUAGE_URL` et
`NUAGE_ADMIN_TOKEN` dans son environnement, il se connecte aussitôt. Depuis les sources :
`cargo run --profile release-editor -p nuage-editor`.

## Variables d'environnement

| Variable | Rôle | Par défaut |
|---|---|---|
| `PORT` | port d'écoute (fixé par Heroku et la plupart des hébergeurs) | `8080` |
| `NUAGE_PG_PORT` | sert aussi le protocole PostgreSQL sur ce port (`5432`) : le pilote PostgreSQL de chaque langage, un jeton comme mot de passe | désactivé |
| `NUAGE_PG_BIND` | son adresse, quand le site reste derrière un proxy sur `127.0.0.1` | `NUAGE_BIND` |
| `NUAGE_TLS_CERT`, `NUAGE_TLS_KEY` | fichiers PEM d'un certificat et de sa clé : TLS pour le protocole PostgreSQL | aucun |
| `NUAGE_TLS_POST_QUANTUM` | `required` : TLS n'accepte que les clients qui conviennent de leurs clés par un échange post-quantique (X25519MLKEM768, TLS 1.3) ; `preferred` : il passe en premier, les clients plus anciens obtiennent X25519 ou P-256 (voir [Contre les ordinateurs quantiques](#contre-les-ordinateurs-quantiques)) | `preferred` |
| `NUAGE_HTTPS_PORT` | HTTPS aussi sur ce port (la console, l'API et le site), avec le certificat de `NUAGE_TLS_CERT` : chiffré de bout en bout sans proxy devant | aucun |
| `NUAGE_BIND` | adresse d'écoute (`127.0.0.1` : cette machine seulement, derrière un proxy) | `0.0.0.0` |
| `NUAGE_DATA_DIR` | répertoire de données | `nuage-data` |
| `NUAGE_NAME` | nom de la première application (et de sa base) | `nuage` |
| `NUAGE_ENCRYPTION_KEY` | la clé qui chiffre tout : 64 chiffres hexadécimaux (`openssl rand -hex 32`, `nuage genkey`), le base64 de 32 octets, ou une phrase de passe d'au moins 12 caractères | **obligatoire** : le binaire ne démarre pas sans |
| `NUAGE_TOKEN` | un jeton de la première application (nommée `NUAGE_NAME`) ; la console crée les autres applications et leurs jetons | aucun |
| `NUAGE_ADMIN_PASSWORD` | mot de passe de la console (et du tableau de bord) | aucun : la console est coupée |
| `NUAGE_ADMIN_TOKEN` | jeton de l'API de gestion (`/api/apps`, `nuage apps`) : crée les applications et leurs jetons ; 32 caractères au moins (`nuage genkey`) | aucun : l'API est coupée |
| `NUAGE_PUBLIC_STATUS` | `true` : `/api/status` et `/app` montrent une partie publique sans connexion | `false` |
| `NUAGE_CACHE_MB` | mémoire que les caches de pages de toutes les applications peuvent occuper ensemble, en Mio, ou `auto` | `auto` : il suit la mémoire : un quart au départ, davantage tant qu'il y a de la place (jusqu'à six dixièmes), moins quand elle se tend |
| `NUAGE_MEMORY_MB` | la mémoire sur laquelle compter, en Mio, pour un hébergeur qui la limite sans le dire | la limite du conteneur, sinon la mémoire de la machine |
| `NUAGE_PG_MAX_CONNECTIONS` | connexions PostgreSQL servies à la fois (chacune a un thread) | `100`, moins là où le système autorise peu de threads |
| `NUAGE_ENV_FILE` | lit aussi les réglages de ce fichier (lignes `CLÉ=valeur` ; `nuage --env-file <fichier>` fait de même) ; l'environnement l'emporte sur le fichier | aucun |
| `NUAGE_SETTINGS` | `environment` : les réglages sont ceux de l'environnement seulement ; ce qui a été enregistré depuis la page Réglages de la console, `nuage settings` ou `/api/settings` est ignoré, et rien ne s'y enregistre (voir [Réglages](#réglages--à-vous-de-choisir-et-de-changer-davis)) | les réglages enregistrés l'emportent sur l'environnement |
| `NUAGE_STATEMENT_MB` | mémoire qu'une instruction SQL peut garder à la fois (les lignes qu'elle trie, joint ou renvoie), en Mio, ou `auto` ; au-delà, l'instruction échoue avec une erreur claire | `auto` : un huitième de la mémoire (32 Mio à 2 Gio) |
| `NUAGE_STATEMENT_SECONDS` | secondes qu'une instruction peut durer avant d'être arrêtée et ses changements annulés ; `0` : pas de limite | `60` |
| `NUAGE_BACKUP_DIR` | sauvegarde vers ce dossier, sur un autre disque (le serveur prévient quand il est sur le disque des données) | |
| `NUAGE_BACKUP_S3_BUCKET` | sauvegarde vers ce bucket | |
| `NUAGE_BACKUP_S3_ENDPOINT` | point d'accès du stockage compatible S3 | `https://s3.<region>.amazonaws.com` |
| `NUAGE_BACKUP_S3_REGION` | région | `us-east-1` |
| `NUAGE_BACKUP_S3_PREFIX` | préfixe des objets de sauvegarde | `nuage/` |
| `NUAGE_BACKUP_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP_S3_SECRET_ACCESS_KEY` | identifiants (sinon `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_BACKUP_SNAPSHOT_MINUTES` | minutes entre deux instantanés (de chaque destination) | `60` |
| `NUAGE_BACKUP2_DIR` | une seconde destination de sauvegarde, tenue à jour avec la première : ce dossier (voir les sauvegardes plus haut) ; seule, c'est l'unique destination | |
| `NUAGE_BACKUP2_S3_BUCKET` | la seconde destination dans ce bucket (avec aussi `NUAGE_BACKUP2_DIR`, ou au même endroit que la première, le serveur ne démarre pas) | |
| `NUAGE_BACKUP2_S3_ENDPOINT`, `NUAGE_BACKUP2_S3_REGION`, `NUAGE_BACKUP2_S3_PREFIX` | son point d'accès, sa région et son préfixe, à donner pour elle : jamais ceux de la première | `https://s3.<region>.amazonaws.com`, `us-east-1`, `nuage/` |
| `NUAGE_BACKUP2_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP2_S3_SECRET_ACCESS_KEY` | ses identifiants (sinon `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_RESTORE_REQUIRED` | `true` : un répertoire de données vide doit être restauré depuis la sauvegarde ; sans rien à restaurer, ou sans sauvegarde, le serveur ne démarre pas, et ne crée ni n'écrit rien (il ne sert pas non plus une application enregistrée dont la sauvegarde ne contient rien). Pour un disque effacé à chaque redémarrage, une fois que le serveur contient des données (voir [Déployer sur Heroku](#déployer-sur-heroku)) | `false` : un démarrage à vide est permis, et dit dans le journal |
| `NUAGE_KV_PORT` | sert aussi le stockage clé-valeur sur un port à lui (`6379`), par le protocole que parlent les clients Redis : on s'y connecte au stockage d'une application, un jeton comme mot de passe | désactivé |
| `NUAGE_KV_BIND` | son adresse | `NUAGE_BIND` |
| `NUAGE_KV_MB` | mémoire du stockage clé-valeur de chaque application, en Mio, ou `auto` ; au-delà, les clés les moins récemment utilisées partent. Ne sert pas avec `NUAGE_KV_STORAGE=database` | `auto` : un seizième de la mémoire, 4 à 256 Mio |
| `NUAGE_KV_STORAGE` | où sont gardés les stockages clé-valeur de toutes les applications : `memory` (le plus rapide ; un redémarrage les vide) ou `database` (dans la base de chaque application : gardés d'un redémarrage à l'autre et dans les sauvegardes ; une écriture attend le disque ; voir [Stockage clé-valeur](#stockage-clé-valeur)) | `memory` |
| `NUAGE_FILES_S3_BUCKET` | garde les fichiers des applications (`/v1/files`) dans ce bucket compatible S3 (voir [Fichiers](#fichiers)) | aucun : pas de fichiers |
| `NUAGE_FILES_S3_ENDPOINT` | son adresse (`https://<id du compte>.r2.cloudflarestorage.com` sur R2) | `https://s3.<région>.amazonaws.com` |
| `NUAGE_FILES_S3_REGION` | sa région | `auto` sur R2, sinon `us-east-1` |
| `NUAGE_FILES_S3_PREFIX` | préfixe des objets : les fichiers d'une application vont sous `<préfixe><application>/` | `files/` |
| `NUAGE_FILES_S3_ACCESS_KEY_ID`, `NUAGE_FILES_S3_SECRET_ACCESS_KEY` | ses identifiants (sinon `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_FILES_LINK_MINUTES` | combien de temps marche un lien de téléchargement ou d'envoi (10080 au plus) | `60` |
| `NUAGE_KEEP_AWAKE_URL` | le serveur appelle cette adresse (son propre `https://…/api/health`) pour qu'un hébergeur qui endort les serveurs inactifs (Heroku Eco) le garde éveillé ; seulement s'il est éveillé : endormi, le visiteur suivant le réveille | coupé |
| `NUAGE_KEEP_AWAKE_MINUTES` | minutes entre deux appels (moins que les 30 d'Eco) | `20` |
| `NUAGE_KEEP_AWAKE_HOURS` | seulement à ces heures, en UTC (`6-22`) ; en dehors, il peut s'endormir | toujours |
| `SITE_URL` | adresse publique du site (liens canoniques, carte de partage) | l'hôte de chaque requête |

Un exemple commenté : [`nuage.env.example`](nuage.env.example). Servez le serveur en HTTPS
(la plupart des hébergeurs le font pour vous ; sur votre serveur, `NUAGE_DOMAIN` de
l'installateur) : le jeton voyage dans un en-tête, et le cookie de la console est `Secure` en
HTTPS.

## `/app` et `/admin`

- **`/app`**, le tableau de bord en direct : requêtes par seconde, latence p50/p99,
  connexions, services, stockage, cache, journal d'écriture, sauvegardes, une carte par application (ses requêtes, sa latence, ses données, ses branches, son chiffrement ; des pastilles restreignent les cartes et le journal à l'une d'elles),
  et un journal d'événements en direct. C'est un îlot Rust/WebAssembly qui lit `/api/status`,
  puis suit `/api/status/stream` (Server-Sent Events, chaque seconde). Il est privé : le lien
  **Se connecter** passe par `/admin?next=/fr/app/` et revient au tableau de bord. Avec
  `NUAGE_PUBLIC_STATUS=true`, les visiteurs voient la partie publique. Il s'installe comme une
  application (PWA) et montre le dernier état connu hors ligne.
- **`/admin`**, la console (en français, anglais et espagnol : la langue du navigateur, ou les liens EN · FR · ES de chaque page) : les applications (des cartes ; en créer une, son jeton s'affiche une fois), puis pour chacune sa vue d'ensemble, ses tables et leurs lignes, ses jetons (nouveau, révocation), ses réglages (description, suppression confirmée par son nom), une console de requêtes (tout
  SQL, avec confirmation avant les modifications), branches, stockage
  et boutons pour un checkpoint, une sauvegarde, une vérification d'intégrité et un
  compactage. Rendue par le serveur, elle fonctionne sans JavaScript, sur téléphone aussi.
  Cookie de session `HttpOnly`, `Secure`, `SameSite=Strict`, jetons CSRF, tentatives de
  connexion limitées.

| Chemin | Contenu | Accès |
|---|---|---|
| `/v1/*` | le protocole de la base, sur la base de l'application qu'ouvre le jeton | `Authorization: Bearer <jeton d'une application>` |
| `POST /v1/sql` | SQL avec paramètres `?`, lignes en objets JSON (sites, tout langage) | un jeton de l'application ; un jeton en lecture seule ne fait que lire |
| `/v1/kv` | le stockage clé-valeur de l'application (voir [Stockage clé-valeur](#stockage-clé-valeur)) | un jeton de l'application ; un jeton en lecture seule lit |
| `/v1/files` | les fichiers de l'application, par des liens signés vers son bucket (voir [Fichiers](#fichiers)) | un jeton de l'application ; un jeton en lecture seule liste et télécharge |
| `/api/health` | `{"status":"ok"}` | public |
| `/api/status`, `/api/status/stream` | statut en direct, en JSON et en Server-Sent Events | jeton d'une application (cette application), jeton de gestion ou session de la console (toutes) |
| `/api/apps` | l'API de gestion : les applications et leurs jetons (voir [Créer une application en une commande](#créer-une-application-en-une-commande)) | `Authorization: Bearer <NUAGE_ADMIN_TOKEN>` |
| `/admin` | la console | `NUAGE_ADMIN_PASSWORD` |
| tout le reste | le site, `/app` compris | public |

## Organisation du dépôt

```
.
├── Cargo.toml            workspace (memoire-orm y est épinglé)
├── crates/
│   ├── nuage/            le moteur, le mode intégré, le serveur, la console, l'API de
│   │                     statut, les sauvegardes (dossier, S3), le binaire `nuage` ;
│   │                     tests/common est la suite de tests des backends de Memoire
│   ├── nuage-site/       le site : pages, tableau de bord /app (Rust/wasm, le bundle
│   │                     précompilé dans pkg/, refait par build-wasm.sh), PWA, carte de partage
│   └── nuage-editor/     Nuage DB Éditeur, les réglages dans une fenêtre (egui) ; packaging/
│                         pour l'application macOS, l'icône Windows et le menu Linux
├── Procfile, RustConfig, heroku-build.sh, app.json    Heroku
├── Dockerfile            tout hébergeur de conteneurs
├── nuage.env.example     l'environnement, commenté
└── .github/workflows/ci.yml
```

La feature `site` de `crates/nuage` (active par défaut) monte le site dans le serveur
(`nuage::site()`) ; `default-features = false` le laisse de côté.

## Développement

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p nuage-site --lib --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace --all-features
cargo run -p nuage-editor           # l'éditeur, face à un serveur que vous lancez
crates/nuage-site/build-wasm.sh   # après une modification du tableau de bord (Rust 1.99, wasm-bindgen de Cargo.lock)
```

## La famille

- [Memoire](https://github.com/maxlestage/Memoire) ([site](https://memoire-7f4525f1fdaf.herokuapp.com/)) : l'ORM Rust dont Nuage DB est la base de données.
- [Paquets](https://github.com/maxlestage/Paquets) ([site](https://paquets-90dfc06b6fea.herokuapp.com/)) : le framework full-stack le plus simple pour Rust.

## Licence

Propriétaire : voir [LICENSE](LICENSE). Copyright (c) 2026 Maxime Nathan Lestage.
