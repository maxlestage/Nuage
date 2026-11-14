<div align="center">

[![Nuage DB](crates/nuage-site/static/og-image.png)](https://github.com/maxlestage/Nuage)

# Nuage DB

**La base de datos de Memoire, en Rust puro. Integrada en tu aplicación o como servidor, con el mismo código.**

[![CI](https://github.com/maxlestage/Nuage/actions/workflows/ci.yml/badge.svg)](https://github.com/maxlestage/Nuage/actions/workflows/ci.yml)
[![Licencia: propietaria](https://img.shields.io/badge/licencia-propietaria-2a2a2e.svg)](#licencia)
[![Desplegar en Heroku](https://www.herokucdn.com/deploy/button.svg)](https://heroku.com/deploy?template=https://github.com/maxlestage/Nuage)

[English](README.md) · [Français](README.fr.md) · **Español**

</div>

---

Nuage DB guarda tus tablas en disco detrás de un registro de escritura anticipada (WAL), crea
al instante ramas copy-on-write de una base, se respalda en una segunda ubicación y se
restaura desde ella al arrancar, y lo sirve todo por HTTP(S) con una consola de
administración y un panel en directo. Es la base de datos de
[Memoire](https://github.com/maxlestage/Memoire), el ORM de Rust: los mismos modelos y
consultas se ejecutan en ella integrada durante el desarrollo y las pruebas, y en un servidor
Nuage DB en producción, sin cambiar una línea.

## Funciones

| | |
|---|---|
| **Cifrado en reposo** | Todo lo almacenado se cifra (`NUAGE_ENCRYPTION_KEY`): las páginas con XTS-AES-256, cada registro del log con AES-256-GCM, y las copias de seguridad, que son copias de ellos. Claves derivadas por aplicación (HKDF, o Argon2id desde una frase de contraseña). |
| **Varias aplicaciones** | Un servidor, una base por aplicación: sus tokens, sus claves, sus copias. El token que envía una aplicación elige su base. La consola y `/app` las muestran una junto a otra. |
| **Páginas en disco** | Tablas e índices en un solo archivo de páginas en árbol B escrito por el propio Nuage DB (copia en escritura, suma de control en cada nodo), con una caché de páginas acotada (`NUAGE_CACHE_MB`): los datos no están limitados por la memoria. Las búsquedas por clave primaria e índice solo leen las filas necesarias. |
| **Registro de escritura** | Cada commit se añade al registro y se sincroniza en disco antes de aplicarse. Checkpoints, recuperación tras un fallo, CRC-32 en los registros y sumas de comprobación en las páginas. |
| **Semántica de Memoire** | Las consultas pasan por el evaluador de Memoire, sobre las filas leídas del disco: Nuage DB se comporta exactamente como el motor en memoria de Memoire, con datos de cualquier tamaño. |
| **Transacciones** | Lecturas sobre una instantánea, commits optimistas (`Error::Conflict`, reintentados por `db.transaction(...)`), transacciones interactivas por HTTP, lotes todo o nada. |
| **Ramas** | Una copia completa de la base para una vista previa, una prueba o un experimento, creada al instante: comparte las páginas de su padre y solo guarda sus propios cambios. |
| **Copias de seguridad restauradas al arrancar** | Instantáneas y segmentos del registro, hacia una carpeta o un almacenamiento compatible con S3. Un directorio de datos vacío se restaura automáticamente al arrancar: los alojamientos con disco efímero funcionan. |
| **Servidor** | Un solo binario, configurado con variables de entorno: el protocolo de la base (`/v1`), el estado en directo (`/api/status`, `/api/status/stream`), la consola (`/admin`) y el sitio web con su panel en directo (`/app`), en un solo puerto. |
| **Cliente WebAssembly** | `memoire_orm::NuageDb` habla HTTP(S): de forma nativa con Tokio, y en el navegador mediante `fetch`. |
| **Mantenimiento automático** | Un hilo en segundo plano hace los checkpoints, envía las copias de seguridad y compacta el archivo. |

## Dos modos, una API

Nuage DB se usa con la API de [Memoire](https://github.com/maxlestage/Memoire): modelos,
constructor de consultas, relaciones, transacciones y migraciones.

```toml
[dependencies]
# El motor, el modo integrado y el servidor. `default-features = false` deja fuera el sitio web.
nuage = { git = "https://github.com/maxlestage/Nuage", default-features = false }
# El mismo Memoire que Nuage DB (la feature `nuage` es el cliente de un servidor).
memoire-orm = { git = "https://github.com/maxlestage/Memoire", rev = "fd1fdd5464547ce1f60f4a11e552c394da73cfc8", features = ["nuage"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### Integrada: un directorio en disco

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
    let db = Embedded::open("data")?; // se crea si no existe
    db.create_table_if_not_exists::<Note>().await?;
    Note { id: 0, text: "hola".into() }.insert(&db).await?;
    println!("{:?}", Note::query().all(&db).await?);
    Ok(())
}
```

### Servidor: el mismo código por HTTP(S)

```rust
let db = NuageDb::connect("https://mi-nuage.herokuapp.com", token).await?;
Note::query().filter(Note::TEXT.contains("hola")).all(&db).await?;
```

### Un solo ajuste para ambos

```rust
// NUAGE_URL=data                                    -> integrada, en ./data
// NUAGE_URL=https://db.example.com NUAGE_TOKEN=...  -> un servidor
let db = nuage::Db::from_env().await?;
```

### Ramas

```rust
db.create_branch("preview-42", None).await?;         // al instante, sea cual sea el tamaño
let preview = db.on_branch("preview-42");
Note { id: 0, text: "solo en la vista previa".into() }.insert(&preview).await?;
db.reset_branch("preview-42").await?;                // olvida sus cambios
db.delete_branch("preview-42").await?;
```

## Cifrado

Nuage DB cifra todo lo que guarda (AES-256, HKDF-SHA256, Argon2id):

| Qué | Cómo |
|---|---|
| páginas (`nuage.db`: tablas, índices, ramas, lista de aplicaciones) | XTS-AES-256 (IEEE 1619), el modo del cifrado de disco: las páginas conservan su lugar y tamaño |
| registro de escritura (`wal/*.log`) | AES-256-GCM por registro, con una clave propia de cada segmento (sal aleatoria), el LSN como nonce |
| copias de seguridad (carpeta o S3) | copias de los archivos anteriores, cifradas igual, con la cabecera del directorio |

`NUAGE_ENCRYPTION_KEY` es una clave (`openssl rand -hex 32`) o una frase de contraseña. Cada
directorio de datos tiene una cabecera, `encryption.json`, con una sal aleatoria y un valor de
control (nunca la clave): su clave raíz es HKDF(clave, sal), o Argon2id(frase, sal) (64 MiB,
3 pasadas), y de ella derivan las claves de las páginas, del registro y el valor de control.
Cada aplicación tiene su directorio, luego su sal y sus claves. Una clave incorrecta se
rechaza al arrancar, antes de leer nada. Una base o una copia guardada en claro por una
versión anterior se cifra al abrirla con una clave, y la copia en claro se sustituye.
**Guarda la clave**: sin ella, ni los datos ni las copias se pueden leer. El modo integrado
también cifra, con `EngineOptions { encryption: Some(Encryption::parse(clave)?), .. }` o
`NUAGE_ENCRYPTION_KEY` con `nuage::Db::from_env()`.

### Viniendo de la versión 0.4

Hasta la 0.4, las páginas eran un archivo [redb](https://docs.rs/redb), `nuage.redb`.
Ahora son las de Nuage DB (`nuage.db`). No hay nada que hacer: un directorio de datos de una
versión anterior, o una copia de seguridad de una de ellas restaurada en un directorio vacío,
se importa la primera vez que esta versión lo abre, cifrado como estaba, y el registro escrito
desde su último punto de control se vuelve a aplicar encima. El archivo antiguo queda junto al
nuevo como `nuage.redb.imported`: bórralo cuando hayas comprobado tus datos. Si hay copias de
seguridad configuradas, se envía enseguida una instantánea de esta versión: la restauración
siguiente ya no pasa por la importación.

La lectura del archivo antiguo es la funcionalidad Cargo `import-redb`, activa por defecto
y con `s3` (una copia de seguridad puede contener una instantánea de esas versiones). El
motor solo (`default-features = false`) no contiene redb en absoluto y rechaza un directorio
así con un mensaje que lo dice: una aplicación que lo integra y tiene datos de esas versiones
añade `features = ["import-redb"]`. Cuando todas las bases hayan sido abiertas por esta
versión, la funcionalidad puede quitarse.

## Varias aplicaciones

Un servidor aloja varias aplicaciones, cada una con su base:

- la **primera aplicación** es la base del servidor (`NUAGE_NAME`, datos en la raíz de
  `NUAGE_DATA_DIR`, token `NUAGE_TOKEN`): un despliegue existente conserva sus datos;
- las **demás** se crean en la consola (`/admin`, **New application**): una base en
  `apps/<nombre>/`, cifrada con sus propias claves, copiada bajo `apps/<nombre>/`, y un primer
  token, mostrado una sola vez (el servidor solo guarda su huella SHA-256);
- el **token** que envía una aplicación elige su base: `NuageDb::connect(url, token)` no
  cambia, y una aplicación nunca ve los datos de otra, ni sus transacciones;
- su lista está en los metadatos de la primera aplicación: registrada, cifrada, copiada y
  restaurada con ella; después cada aplicación restaura su propia copia.

`Server::create_app`, `new_app_token`, `revoke_app_token`, `delete_app` y `apps` hacen lo
mismo desde Rust.

## SQL

Nuage DB habla SQL (`POST /v1/sql`, la pestaña **Query** de la consola, `Embedded::sql` en Rust):

| | |
|---|---|
| **Leer** | `SELECT [DISTINCT]`, `JOIN`, `LEFT JOIN`, `CROSS JOIN` (`ON`, `USING`), `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY ... NULLS FIRST/LAST`, `LIMIT`/`OFFSET`, `UNION [ALL]`, `INTERSECT`, `EXCEPT`, subconsultas (`IN`, `EXISTS`, escalares, correlacionadas, en `FROM`) |
| **Expresiones** | `CASE`, `CAST` y `::`, `\|\|`, `+ - * / %`, `LIKE`, `ILIKE`, `BETWEEN`, `IN`, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM` |
| **Funciones** | `count`, `sum`, `avg`, `min`, `max`, `total`, `group_concat`/`string_agg`, `array_agg` (con `DISTINCT`); `lower`, `upper`, `length`, `trim`, `substr`, `replace`, `instr`, `lpad`, `concat`, `coalesce`, `nullif`, `iif`, `greatest`, `least`, `abs`, `round`, `floor`, `ceil`, `power`, `sqrt`, `random`, `now`, `current_date`, `date`, `extract`, `strftime`, `json_extract`, `json_object`, `json_array`, `gen_random_uuid`... |
| **Escribir** | `INSERT ... VALUES / SELECT / DEFAULT VALUES`, `ON CONFLICT ... DO NOTHING / DO UPDATE SET ... excluded.x`, `INSERT OR IGNORE`, `REPLACE INTO`, `ON DUPLICATE KEY UPDATE`, `UPDATE ... SET col = <expresión>`, `DELETE`, `RETURNING` |
| **Esquema** | `CREATE TABLE [IF NOT EXISTS]` (tipos de PostgreSQL, MySQL y SQLite; `PRIMARY KEY`, `SERIAL`/`AUTOINCREMENT`, `NOT NULL`, `UNIQUE`, `DEFAULT`, `REFERENCES`, `UNIQUE (a, b)`), `CREATE TABLE ... AS SELECT`, `CREATE [UNIQUE] INDEX` (construido sobre las filas existentes), `DROP INDEX`, `ALTER TABLE ... ADD COLUMN`, `DROP TABLE`, `EXPLAIN` |

Una consulta que las instrucciones de Memoire saben expresar se ejecuta como una de ellas, por
los índices, deteniéndose al final de la página; el resto (funciones, `CASE`, subconsultas,
`UNION`) lo calcula la capa SQL sobre las filas encontradas por los índices, con joins por
hash. No están: joins `RIGHT`/`FULL`, `WITH`, funciones de ventana, vistas, disparadores, clave
primaria de varias columnas, cambiar una clave primaria. Una tabla creada sin clave primaria
recibe una columna `rowid`.

## Cualquier lenguaje

Nuage DB habla SQL por HTTP (`POST /v1/sql`, JSON de ida y de vuelta): todos los lenguajes
lo usan, sin instalar nada. [`clients/`](clients) tiene un ejemplo completo, probado contra un
servidor real en CI, para curl, JavaScript, TypeScript, Python, PHP, Go, Ruby, Java, Kotlin,
C#, Swift, Dart (Flutter), Elixir, Rust, C y Perl; la consola muestra la misma petición en cada lenguaje con la dirección del servidor.
Cada servidor describe su API en `/v1/openapi.json` (OpenAPI 3.1) para generar un cliente en
cualquier otro lenguaje. Rust tiene además el cliente nativo, `NuageDb`. Y
cada driver PostgreSQL se conecta también: ver abajo.

**¿Funciona mi conexión?** `GET /v1/ping` responde qué aplicación abre el token:
`curl https://db.example.com/v1/ping -H "Authorization: Bearer <token>"` da
`Connected to Nuage DB 0.4.0: application blog, read and write, branch main, 3 table(s).`,
y sin token, o con uno que no abre nada, dice qué falla. Desde cualquier máquina:
`nuage check <url> <token>`. Una conexión PostgreSQL recibe la misma frase como aviso (psql
la muestra), y la página de cada aplicación en la consola muestra su última conexión (cuándo,
cómo, qué cliente), o que espera la primera.

## Protocolo PostgreSQL

Nuage DB habla también el protocolo de PostgreSQL (`NUAGE_PG_PORT=5432`; el instalador y
Docker lo activan): **el driver PostgreSQL, el ORM y la herramienta de cada lenguaje se
conectan tal cual**, con cualquier nombre de usuario y **un token de aplicación como
contraseña**; el nombre de la base elige una rama (el nombre de la aplicación, o `main`: la
principal).

```bash
PGPASSWORD=<token> psql "host=db.example.com port=5432 user=app dbname=nuage sslmode=require"
# postgresql://app:<token>@db.example.com:5432/nuage?sslmode=require
```

Probado en CI con psql, psycopg 2 y 3, SQLAlchemy, node-postgres, Postgres.js, pgx,
database/sql de Go, JDBC, PDO, la gema pg, tokio-postgres y Npgsql: ejemplos en
[`clients/postgres/`](clients/postgres). Sentencias preparadas, parámetros `$1` (su tipo
viene de las columnas con las que se encuentran), valores en texto y binario, `BEGIN` ...
`COMMIT` entre mensajes, savepoints, códigos de error de PostgreSQL (`40001` en un conflicto
de escritura: vuelve a ejecutar la transacción). `SELECT ... FOR UPDATE` (`SKIP LOCKED`, `NOWAIT`, `FOR SHARE`) y los bloqueos consultivos (`pg_advisory_xact_lock`, `hashtextextended`) se aceptan: una transacción ya falla en su commit (`40001`) si otra modificó una tabla que leyó. `x IN (SELECT jsonb_array_elements_text($1::jsonb)::bigint)` recibe una lista en un solo parámetro JSON. TLS con `NUAGE_TLS_CERT` y `NUAGE_TLS_KEY`
(el instalador usa el certificado del dominio). El SQL es el de Nuage DB (arriba); los
catálogos del sistema (`pg_catalog`) no están: las herramientas que los recorren (pgAdmin)
muestran poco, y Npgsql lleva `Server Compatibility Mode=NoTypeLoading`. Heroku solo
enruta HTTP: allí, usa `/v1/sql`.

## Sitios web

Cualquier sitio puede usar la base de una aplicación, desde su servidor o desde sus páginas:

- **`POST /v1/sql`**, desde cualquier lenguaje: SQL completo con parámetros `?` (o `$1`),
  valores nunca leídos como SQL (sin inyección). Las filas vuelven como objetos JSON
  (`"arrays": true` para arrays); un insert devuelve su `last_insert_id`. Varias instrucciones
  (separadas por `;`, o `"statements": [...]`) forman una sola transacción, todo o nada.
- **Desde el navegador**: añade el sitio en la consola (**Settings → Websites**:
  `https://sitio.es`, `http://localhost:3000`, o `*`) y sus páginas pueden llamar a `/v1`
  (CORS). La petición previa del navegador se guarda en caché un día: un viaje, una sola vez.
- **Tokens de solo lectura** (**Tokens → New read-only token**): un token en una página es
  público, así que las páginas reciben uno que lee y nunca escribe; los tokens de lectura y
  escritura se quedan en servidores.

```js
const res = await fetch("https://mi-nuage.herokuapp.com/v1/sql", {
  method: "POST",
  headers: { "Authorization": "Bearer " + TOKEN_LECTURA, "Content-Type": "application/json" },
  body: JSON.stringify({ sql: "SELECT id, title FROM posts WHERE category = ? ORDER BY id DESC LIMIT 20", params: [3] }),
});
const { rows } = await res.json(); // [{ "id": 42, "title": "…" }, …]
```

```bash
curl https://mi-nuage.herokuapp.com/v1/sql -H "Authorization: Bearer $NUAGE_TOKEN" \
  -d '{"sql": "INSERT INTO messages (name, text) VALUES (?, ?)", "params": ["Ada", "Hola"]}'
```

Velocidad, medida en la máquina del servidor (`cargo run --release -p nuage --example bench`,
datos cifrados, 20 000 filas, 4 núcleos): una fila por clave o por columna única en
**0,2 ms**, una página de 20 filas por un índice en **0,5 ms**, una actualización duradera
(fsync) en **0,5 ms**, **más de 13 000 lecturas/s**. Las respuestas JSON de 1 KiB o más se
comprimen con gzip para los clientes que lo aceptan. Lo que nota un visitante es sobre todo
la red: elige la región del proveedor más cercana a él, y un dyno que nunca se duerme
(Heroku **Basic**, no Eco, cuya primera petición tras una pausa tarda varios segundos).

## Instalar en un servidor

En cualquier servidor Linux (Debian, Ubuntu, Fedora, Rocky, Alpine..., x86_64 o ARM64, desde
512 MiB de memoria), un comando:

```bash
curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh | sudo bash
```

Instala el binario `nuage` (compilado estático: sin dependencias), **crea swap** si el
servidor no tiene (el tamaño de la memoria, de 1 a 4 GiB; `vm.swappiness=10`, para que la
memoria sea para la base), un usuario `nuage`, los datos en `/var/lib/nuage`, los ajustes en
`/etc/nuage/nuage.env` con una clave, un token y una contraseña de consola generados, y un
servicio systemd que arranca con el sistema. Muestra una vez la dirección, el token y la
contraseña: **copia `/etc/nuage/nuage.env` en un lugar seguro**, nadie puede leer los datos
sin su clave.

- **HTTPS**: apunta un nombre de dominio al servidor y ejecuta
  `curl -fsSL .../install.sh | sudo NUAGE_DOMAIN=db.example.com bash` (Caddy, con un
  certificado gratuito; Nuage DB escucha entonces solo en `127.0.0.1`). Sin HTTPS la consola
  funciona, pero avisa de que la contraseña viaja sin cifrar.
- **Actualizar**: vuelve a ejecutar el comando; datos, clave y ajustes se conservan.
- **Opciones**: `NUAGE_PORT`, `NUAGE_SWAP_MB` (`0`: sin swap), `NUAGE_VERSION=v0.3.0`,
  `NUAGE_FROM_SOURCE=1` (compilar con cargo).
- **Desinstalar**: `... | sudo bash -s uninstall` (conserva los datos),
  `... | sudo bash -s purge`.
- **Memoria**: la caché de páginas de cada aplicación es `NUAGE_CACHE_MB=auto`, un octavo de
  la memoria (de 8 a 512 MiB); el resto queda en disco. La página *Servidor* de la consola
  muestra la memoria que usa Nuage DB, la memoria libre y el swap.

Hay binarios para Linux, macOS y Windows en la
[página de versiones](https://github.com/maxlestage/Nuage/releases).

## Inicio rápido

```bash
git clone https://github.com/maxlestage/Nuage && cd Nuage
# La clave que lo cifra todo: guárdala, sin ella los datos son ilegibles.
export NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32)
NUAGE_TOKEN=dev-token NUAGE_ADMIN_PASSWORD=dev-password cargo run -p nuage --release
# nuage: data is stored in /home/yo/Nuage/nuage-data on this server's disk
# nuage: listening on 0.0.0.0:8080
```

Después abre:

- http://localhost:8080/ : el sitio web (inglés, francés, español);
- http://localhost:8080/app : el panel en directo (inicia sesión con `NUAGE_ADMIN_PASSWORD`);
- http://localhost:8080/admin : la consola de administración.

Las aplicaciones se conectan con `NuageDb::connect("http://localhost:8080", "dev-token")`.

Docker (Compose, con los ajustes en `.env`, a partir de [`nuage.env.example`](nuage.env.example)):

```bash
cp nuage.env.example .env   # y define NUAGE_ENCRYPTION_KEY, NUAGE_TOKEN, NUAGE_ADMIN_PASSWORD
docker compose up -d
```

## Desplegar en Heroku

La raíz de este repositorio se despliega tal cual: `Procfile`, `RustConfig` y
`heroku-build.sh` (el buildpack `emk/rust` compila el binario `nuage` con el sitio web), y
`app.json` para el botón **Deploy to Heroku** de arriba.

- **Un dyno Basic.** Un dyno Basic nunca se duerme. Ejecuta exactamente un dyno web: dos
  dynos serían dos bases distintas escribiendo en las mismas copias de seguridad.
- **El disco es efímero.** El disco de un dyno se borra en cada reinicio (al menos una vez al
  día, y en cada despliegue). El directorio de datos es solo una copia de trabajo:
  **configura las copias de seguridad**, y cada arranque restaura la base a partir de ellas.
  En Heroku, usa un almacenamiento compatible con S3 (`NUAGE_BACKUP_S3_*`); en un alojamiento
  con disco persistente, basta con una carpeta (`NUAGE_BACKUP_DIR`).
- **Ventana de pérdida.** Los commits hechos desde el último segmento del registro enviado
  (30 segundos por defecto) solo se pierden si el dyno se detiene sin el `SIGTERM` que Heroku
  envía antes: con `SIGTERM`, el servidor hace un checkpoint y envía el último segmento.

A mano:

```bash
heroku create mi-nuage --buildpack emk/rust
heroku config:set NUAGE_ENCRYPTION_KEY=$(openssl rand -hex 32) NUAGE_TOKEN=$(openssl rand -hex 32) \
  NUAGE_ADMIN_PASSWORD='una frase de contraseña larga' \
  NUAGE_BACKUP_S3_BUCKET=mi-bucket NUAGE_BACKUP_S3_REGION=eu-west-3 \
  NUAGE_BACKUP_S3_ACCESS_KEY_ID=... NUAGE_BACKUP_S3_SECRET_ACCESS_KEY=...
git push heroku master
```

Para un almacenamiento distinto de AWS, define también `NUAGE_BACKUP_S3_ENDPOINT`. Después
abre `https://mi-nuage.herokuapp.com/admin`.

## Variables de entorno

| Variable | Función | Por defecto |
|---|---|---|
| `PORT` | puerto de escucha (lo fija Heroku y la mayoría de alojamientos) | `8080` |
| `NUAGE_PG_PORT` | sirve también el protocolo PostgreSQL en este puerto (`5432`): el driver PostgreSQL de cada lenguaje, un token como contraseña | apagado |
| `NUAGE_PG_BIND` | su dirección, cuando el sitio queda detrás de un proxy en `127.0.0.1` | `NUAGE_BIND` |
| `NUAGE_TLS_CERT`, `NUAGE_TLS_KEY` | archivos PEM de un certificado y su clave: TLS para el protocolo PostgreSQL | ninguno |
| `NUAGE_BIND` | dirección de escucha (`127.0.0.1`: solo esta máquina, detrás de un proxy) | `0.0.0.0` |
| `NUAGE_DATA_DIR` | directorio de datos | `nuage-data` |
| `NUAGE_NAME` | nombre de la primera aplicación (y de su base) | `nuage` |
| `NUAGE_ENCRYPTION_KEY` | la clave que lo cifra todo: 64 dígitos hexadecimales (`openssl rand -hex 32`, `nuage genkey`), el base64 de 32 bytes, o una frase de al menos 12 caracteres | **obligatoria**: el binario no arranca sin ella |
| `NUAGE_TOKEN` | un token de la primera aplicación (llamada `NUAGE_NAME`); la consola crea las demás aplicaciones y sus tokens | ninguno |
| `NUAGE_ADMIN_PASSWORD` | contraseña de la consola (y del panel) | ninguna: la consola está desactivada |
| `NUAGE_PUBLIC_STATUS` | `true`: `/api/status` y `/app` muestran una parte pública sin iniciar sesión | `false` |
| `NUAGE_CACHE_MB` | caché de páginas de cada aplicación, en MiB, o `auto` | `auto`: un octavo de la memoria, de 8 a 512 MiB |
| `NUAGE_BACKUP_DIR` | copias de seguridad en esta carpeta | |
| `NUAGE_BACKUP_S3_BUCKET` | copias de seguridad en este bucket | |
| `NUAGE_BACKUP_S3_ENDPOINT` | endpoint del almacenamiento compatible con S3 | `https://s3.<region>.amazonaws.com` |
| `NUAGE_BACKUP_S3_REGION` | región | `us-east-1` |
| `NUAGE_BACKUP_S3_PREFIX` | prefijo de los objetos de copia | `nuage/` |
| `NUAGE_BACKUP_S3_ACCESS_KEY_ID`, `NUAGE_BACKUP_S3_SECRET_ACCESS_KEY` | credenciales (si no, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`) | |
| `NUAGE_BACKUP_SNAPSHOT_MINUTES` | minutos entre dos instantáneas | `60` |
| `NUAGE_KEEP_AWAKE_URL` | el servidor llama a esta dirección (su propio `https://…/api/health`) para que un proveedor que duerme los servidores inactivos (Heroku Eco) lo mantenga despierto; solo si está despierto: dormido, el siguiente visitante lo despierta | desactivado |
| `NUAGE_KEEP_AWAKE_MINUTES` | minutos entre dos llamadas (menos de los 30 de Eco) | `20` |
| `NUAGE_KEEP_AWAKE_HOURS` | solo a esas horas, en UTC (`6-22`); fuera de ellas puede dormirse | siempre |
| `SITE_URL` | dirección pública del sitio (enlaces canónicos, tarjeta para compartir) | el host de cada petición |

Un ejemplo comentado: [`nuage.env.example`](nuage.env.example). Sirve el servidor por HTTPS
(la mayoría de alojamientos lo hacen por ti; en tu propio servidor, `NUAGE_DOMAIN` del
instalador): el token viaja en una cabecera, y la cookie de la consola es `Secure` por HTTPS.

## `/app` y `/admin`

- **`/app`**, el panel en directo: peticiones por segundo, latencia p50/p99, conexiones,
  servicios, almacenamiento, caché, registro de escritura, copias de seguridad, una tarjeta por aplicación (sus peticiones, latencia, datos, ramas y cifrado; unas pastillas limitan las tarjetas y el registro a una de ellas),
  y un registro de eventos en directo. Es una isla Rust/WebAssembly que lee `/api/status` y
  luego sigue `/api/status/stream` (Server-Sent Events, cada segundo). Es privado: el enlace
  **Iniciar sesión** pasa por `/admin?next=/es/app/` y vuelve al panel. Con
  `NUAGE_PUBLIC_STATUS=true`, los visitantes ven la parte pública. Se instala como una
  aplicación (PWA) y muestra el último estado conocido sin conexión.
- **`/admin`**, la consola (en español, inglés y francés: el idioma del navegador, o los enlaces EN · FR · ES de cada página): las aplicaciones (tarjetas; crear una, su token se muestra una vez), y para cada una su vista general, sus tablas y sus filas, sus tokens (nuevo, revocar), sus ajustes (descripción, borrado confirmado con su nombre), consola de consultas (todo
  SQL, con confirmación antes de los cambios), ramas, almacenamiento y
  botones para un checkpoint, una copia de seguridad, una comprobación de integridad y una
  compactación. Renderizada por el servidor, funciona sin JavaScript, también en el móvil.
  Cookie de sesión `HttpOnly`, `Secure`, `SameSite=Strict`, tokens CSRF, intentos de inicio
  de sesión limitados.

| Ruta | Contenido | Acceso |
|---|---|---|
| `/v1/*` | el protocolo de la base, sobre la base de la aplicación que abre el token | `Authorization: Bearer <token de una aplicación>` |
| `POST /v1/sql` | SQL con parámetros `?`, filas como objetos JSON (sitios, cualquier lenguaje) | un token de la aplicación; un token de solo lectura solo lee |
| `/api/health` | `{"status":"ok"}` | público |
| `/api/status`, `/api/status/stream` | estado en directo, en JSON y en Server-Sent Events | token de una aplicación (esa aplicación) o sesión de la consola (todas) |
| `/admin` | la consola | `NUAGE_ADMIN_PASSWORD` |
| todo lo demás | el sitio web, `/app` incluido | público |

## Estructura del repositorio

```
.
├── Cargo.toml            workspace (memoire-orm se fija aquí)
├── crates/
│   ├── nuage/            el motor, el modo integrado, el servidor, la consola, la API de
│   │                     estado, las copias (carpeta, S3), el binario `nuage`;
│   │                     tests/common es la suite de pruebas de backends de Memoire
│   └── nuage-site/       el sitio web: páginas, el panel /app (Rust/wasm, el bundle
│                         precompilado en pkg/, regenerado por build-wasm.sh), PWA, tarjeta para compartir
├── Procfile, RustConfig, heroku-build.sh, app.json    Heroku
├── Dockerfile            cualquier alojamiento de contenedores
├── nuage.env.example     el entorno, comentado
└── .github/workflows/ci.yml
```

La feature `site` de `crates/nuage` (activa por defecto) monta el sitio web en el servidor
(`nuage::site()`); `default-features = false` lo deja fuera.

## Desarrollo

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p nuage-site --lib --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace --all-features
crates/nuage-site/build-wasm.sh   # tras cambiar el panel (Rust 1.99, wasm-bindgen de Cargo.lock)
```

## La familia

- [Memoire](https://github.com/maxlestage/Memoire) ([sitio](https://memoire-7f4525f1fdaf.herokuapp.com/)): el ORM de Rust cuya base de datos es Nuage DB.
- [Paquets](https://github.com/maxlestage/Paquets) ([sitio](https://paquets-90dfc06b6fea.herokuapp.com/)): el framework full-stack más sencillo para Rust.

## Licencia

Propietaria: consulta [LICENSE](LICENSE). Copyright (c) 2026 Maxime Nathan Lestage.
