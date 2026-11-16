//! The console in English, French and Spanish.
//!
//! Texts are written in English in the code; [`t`] gives them in the
//! session's language, [`tf`] fills their `{}` in order. A text without a
//! translation is a bug: debug builds (and the tests) stop on it, release
//! builds show the English.

use std::collections::HashMap;
use std::sync::OnceLock;

use hyper::header::{self, HeaderMap};

/// A language of the console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lang {
    En,
    Fr,
    Es,
}

impl Lang {
    pub(crate) const ALL: [Lang; 3] = [Lang::En, Lang::Fr, Lang::Es];

    pub(crate) fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Fr => "fr",
            Lang::Es => "es",
        }
    }

    pub(crate) fn from_code(code: &str) -> Option<Lang> {
        let code = code.trim().to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| code == l.code() || code.starts_with(&format!("{}-", l.code())))
    }

    /// The language of a request: the `nuage_lang` cookie, else the first of
    /// `Accept-Language` the console speaks, else English.
    pub(crate) fn of(headers: &HeaderMap) -> Lang {
        if let Some(l) = super::admin::cookie(headers, super::admin::LANG_COOKIE).and_then(Lang::from_code) {
            return l;
        }
        let accept = headers.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).unwrap_or_default();
        let mut best: Option<(f32, Lang)> = None;
        for part in accept.split(',') {
            let mut bits = part.split(';');
            let tag = bits.next().unwrap_or_default();
            let q = bits.find_map(|b| b.trim().strip_prefix("q=")).and_then(|q| q.parse::<f32>().ok()).unwrap_or(1.0);
            if let Some(l) = Lang::from_code(tag)
                && best.is_none_or(|(bq, _)| q > bq)
            {
                best = Some((q, l));
            }
        }
        best.map_or(Lang::En, |(_, l)| l)
    }
}

/// `en` has its French and its Spanish.
#[cfg(test)]
pub(crate) fn translated(en: &str) -> bool {
    table().contains_key(en)
}

/// `en` in the language `l`.
pub(crate) fn t(l: Lang, en: &'static str) -> &'static str {
    if l == Lang::En {
        return en;
    }
    match table().get(en) {
        Some((fr, es)) => {
            if l == Lang::Fr {
                fr
            } else {
                es
            }
        }
        None => {
            debug_assert!(false, "no {} translation for {en:?}", l.code());
            en
        }
    }
}

/// [`t`], its `{}` filled with `args` in order.
pub(crate) fn tf(l: Lang, en: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    let template = t(l, en);
    let mut out = String::with_capacity(template.len() + 16);
    let mut args = args.iter();
    let mut rest = template;
    while let Some(i) = rest.find("{}") {
        out.push_str(&rest[..i]);
        if let Some(a) = args.next() {
            out.push_str(&a.to_string());
        }
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out
}

fn table() -> &'static HashMap<&'static str, (&'static str, &'static str)> {
    static TABLE: OnceLock<HashMap<&'static str, (&'static str, &'static str)>> = OnceLock::new();
    TABLE.get_or_init(|| TEXTS.iter().map(|(en, fr, es)| (*en, (*fr, *es))).collect())
}

/// English, French, Spanish.
const TEXTS: &[(&str, &str, &str)] = &[
    // Navigation, login
    ("Applications", "Applications", "Aplicaciones"),
    ("Application", "Application", "Aplicación"),
    ("Server", "Serveur", "Servidor"),
    ("Log out", "Se déconnecter", "Cerrar sesión"),
    ("Console", "Console", "Consola"),
    ("Password", "Mot de passe", "Contraseña"),
    ("Log in", "Se connecter", "Iniciar sesión"),
    ("Log in first.", "Connectez-vous d'abord.", "Inicia sesión primero."),
    ("Invalid request.", "Requête invalide.", "Petición no válida."),
    ("This form is too large.", "Ce formulaire est trop grand.", "Este formulario es demasiado grande."),
    (
        "Too many failed attempts. Wait a few minutes.",
        "Trop d'essais manqués. Attendez quelques minutes.",
        "Demasiados intentos fallidos. Espera unos minutos.",
    ),
    (
        "The form expired. Try again.",
        "Le formulaire a expiré. Réessayez.",
        "El formulario ha caducado. Inténtalo de nuevo.",
    ),
    ("Wrong password.", "Mot de passe incorrect.", "Contraseña incorrecta."),
    (
        "Set <code>NUAGE_PG_PORT=5432</code> and every language's PostgreSQL driver connects too (psql, psycopg, JDBC, node-postgres, pgx, Npgsql, PDO, the pg gem...).",
        "Avec <code>NUAGE_PG_PORT=5432</code>, le pilote PostgreSQL de chaque langage se connecte aussi (psql, psycopg, JDBC, node-postgres, pgx, Npgsql, PDO, le gem pg...).",
        "Con <code>NUAGE_PG_PORT=5432</code>, el driver PostgreSQL de cada lenguaje se conecta también (psql, psycopg, JDBC, node-postgres, pgx, Npgsql, PDO, la gema pg...).",
    ),
    (
        "Every language's PostgreSQL driver, ORM and tool connects: any user name, a token of this application as the password. The database name picks a branch.",
        "Le pilote PostgreSQL, l'ORM et l'outil de chaque langage se connectent : un nom d'utilisateur quelconque, un jeton de cette application comme mot de passe. Le nom de base choisit une branche.",
        "El driver PostgreSQL, el ORM y la herramienta de cada lenguaje se conectan: cualquier nombre de usuario, un token de esta aplicación como contraseña. El nombre de la base elige una rama.",
    ),
    (
        "Without TLS the token travels in clear: set NUAGE_TLS_CERT and NUAGE_TLS_KEY (the installer does it with a domain name).",
        "Sans TLS le jeton circule en clair : définissez NUAGE_TLS_CERT et NUAGE_TLS_KEY (l'installateur le fait avec un nom de domaine).",
        "Sin TLS el token viaja sin cifrar: define NUAGE_TLS_CERT y NUAGE_TLS_KEY (el instalador lo hace con un nombre de dominio).",
    ),
    (
        r#"Complete examples (also TypeScript, Perl and C), tested against a real server: <a href="https://github.com/maxlestage/Nuage/tree/master/clients">clients/</a>. To generate a client in another language: <a href="/v1/openapi.json">/v1/openapi.json</a> (OpenAPI)."#,
        r#"Des exemples complets (aussi TypeScript, Perl et C), testés sur un vrai serveur : <a href="https://github.com/maxlestage/Nuage/tree/master/clients">clients/</a>. Pour générer un client dans un autre langage : <a href="/v1/openapi.json">/v1/openapi.json</a> (OpenAPI)."#,
        r#"Ejemplos completos (también TypeScript, Perl y C), probados contra un servidor real: <a href="https://github.com/maxlestage/Nuage/tree/master/clients">clients/</a>. Para generar un cliente en otro lenguaje: <a href="/v1/openapi.json">/v1/openapi.json</a> (OpenAPI)."#,
    ),
    (
        "This connection is not encrypted (HTTP): the password travels in clear. Reach the console over HTTPS (a domain name with the installer's NUAGE_DOMAIN, or a proxy), or through an SSH tunnel to localhost.",
        "Cette connexion n'est pas chiffrée (HTTP) : le mot de passe circule en clair. Ouvrez la console en HTTPS (un nom de domaine avec NUAGE_DOMAIN de l'installateur, ou un proxy), ou par un tunnel SSH vers localhost.",
        "Esta conexión no está cifrada (HTTP): la contraseña viaja sin cifrar. Abre la consola por HTTPS (un nombre de dominio con NUAGE_DOMAIN del instalador, o un proxy), o por un túnel SSH a localhost.",
    ),
    ("Console off", "Console coupée", "Consola desactivada"),
    ("The console is off", "La console est coupée", "La consola está desactivada"),
    (
        "Set <code>NUAGE_ADMIN_PASSWORD</code> on the server to turn it on.",
        "Définissez <code>NUAGE_ADMIN_PASSWORD</code> sur le serveur pour l'activer.",
        "Define <code>NUAGE_ADMIN_PASSWORD</code> en el servidor para activarla.",
    ),
    ("Refused", "Refusé", "Rechazado"),
    (
        "Invalid form token (CSRF). Reload the page and try again.",
        "Jeton de formulaire invalide (CSRF). Rechargez la page et réessayez.",
        "Token de formulario no válido (CSRF). Recarga la página e inténtalo de nuevo.",
    ),
    ("Error", "Erreur", "Error"),
    ("Not found", "Introuvable", "No encontrado"),
    ("No such application", "Cette application n'existe pas", "Esa aplicación no existe"),
    ("Home", "Accueil", "Inicio"),
    ("Nuage DB is running", "Nuage DB fonctionne", "Nuage DB está en marcha"),
    (
        "{} application(s), version {}. Everything stored is encrypted.",
        "{} application(s), version {}. Tout ce qui est stocké est chiffré.",
        "{} aplicación(es), versión {}. Todo lo almacenado está cifrado.",
    ),
    ("Open the console", "Ouvrir la console", "Abrir la consola"),
    ("Health", "Santé", "Salud"),
    // Applications
    (
        "{} application(s) on this server. Each one has its own database, tokens, encryption keys and backups.",
        "{} application(s) sur ce serveur. Chacune a sa base, ses jetons, ses clés de chiffrement et ses sauvegardes.",
        "{} aplicación(es) en este servidor. Cada una tiene su base, sus tokens, sus claves de cifrado y sus copias.",
    ),
    ("first", "première", "primera"),
    ("first application", "première application", "primera aplicación"),
    ("encrypted", "chiffrée", "cifrada"),
    ("in clear", "en clair", "sin cifrar"),
    ("cannot be opened", "impossible à ouvrir", "no se puede abrir"),
    ("Backup failing: {}", "Sauvegarde en échec : {}", "Copia con errores: {}"),
    ("New application", "Nouvelle application", "Nueva aplicación"),
    ("Name", "Nom", "Nombre"),
    ("Description", "Description", "Descripción"),
    ("optional", "facultatif", "opcional"),
    ("Create", "Créer", "Crear"),
    (
        "Its database is created at once, encrypted with keys of its own, with a first token.",
        "Sa base est créée aussitôt, chiffrée avec ses propres clés, avec un premier jeton.",
        "Su base se crea al instante, cifrada con sus propias claves, con un primer token.",
    ),
    (
        "Application {} created, with its own encrypted database.",
        "Application {} créée, avec sa propre base chiffrée.",
        "Aplicación {} creada, con su propia base cifrada.",
    ),
    (
        "Application {} deleted, with its data and backups.",
        "Application {} supprimée, avec ses données et ses sauvegardes.",
        "Aplicación {} borrada, con sus datos y sus copias.",
    ),
    ("There is no application {}.", "Il n'y a pas d'application {}.", "No hay ninguna aplicación {}."),
    ("Other applications:", "Autres applications :", "Otras aplicaciones:"),
    // Tabs
    ("Overview", "Vue d'ensemble", "Resumen"),
    ("Tables", "Tables", "Tablas"),
    ("Query", "Requêtes SQL", "Consultas SQL"),
    ("Branches", "Branches", "Ramas"),
    ("Storage", "Stockage", "Almacenamiento"),
    ("Tokens", "Jetons", "Tokens"),
    ("Settings", "Réglages", "Ajustes"),
    // Figures
    ("Size on disk", "Taille sur disque", "Tamaño en disco"),
    ("Size", "Taille", "Tamaño"),
    ("Rows", "Lignes", "Filas"),
    ("Requests", "Requêtes", "Peticiones"),
    ("p99", "p99", "p99"),
    ("Latency", "Latence", "Latencia"),
    ("Errors", "Erreurs", "Errores"),
    ("Last request", "Dernière requête", "Última petición"),
    ("Last connection", "Dernière connexion", "Última conexión"),
    ("HTTP API", "API HTTP", "API HTTP"),
    ("SQL over HTTP", "SQL en HTTP", "SQL por HTTP"),
    ("PostgreSQL", "PostgreSQL", "PostgreSQL"),
    ("Refresh", "Actualiser", "Actualizar"),
    ("{} used, {} free of {}", "{} utilisés, {} libres sur {}", "{} usados, {} libres de {}"),
    ("the limit of this server", "la limite de ce serveur", "el límite de este servidor"),
    ("not reported by the host", "non indiqué par l'hébergeur", "no indicado por el alojamiento"),
    ("Page caches", "Caches de pages", "Cachés de páginas"),
    ("Time", "Heure", "Hora"),
    ("Level", "Niveau", "Nivel"),
    ("Event", "Événement", "Evento"),
    (
        "{} for all the applications (up to {} while memory has room), {} each",
        "{} pour toutes les applications (jusqu'à {} tant que la mémoire le permet), {} chacune",
        "{} para todas las aplicaciones (hasta {} mientras la memoria lo permita), {} cada una",
    ),
    ("Light", "Clair", "Claro"),
    ("Dark", "Sombre", "Oscuro"),
    ("Auto", "Auto", "Auto"),
    ("Language", "Langue", "Idioma"),
    ("Theme", "Thème", "Tema"),
    ("As the device", "Comme l'appareil", "Como el dispositivo"),
    ("Connected: last connection {}.", "Connectée : dernière connexion {}.", "Conectada: última conexión {}."),
    ("Waiting for a first connection.", "En attente d'une première connexion.", "Esperando una primera conexión."),
    (
        "Test yours: the server answers which application the token opens, and this page shows it as soon as it arrives.",
        "Testez la vôtre : le serveur répond quelle application le jeton ouvre, et cette page l'affiche dès qu'elle arrive.",
        "Prueba la tuya: el servidor responde qué aplicación abre el token, y esta página la muestra en cuanto llega.",
    ),
    (
        "Test it: the server answers which application the token opens, and this page shows the connection.",
        "Testez-le : le serveur répond quelle application le jeton ouvre, et cette page affiche la connexion.",
        "Pruébalo: el servidor responde qué aplicación abre el token, y esta página muestra la conexión.",
    ),
    ("Encryption", "Chiffrement", "Cifrado"),
    ("Last checkpoint", "Dernier checkpoint", "Último checkpoint"),
    ("Backup", "Sauvegarde", "Copia de seguridad"),
    ("Created", "Créée", "Creada"),
    ("{}/s, {} in all", "{}/s, {} au total", "{}/s, {} en total"),
    ("on ({}), keys of its own", "activé ({}), clés propres", "activado ({}), claves propias"),
    ("key", "clé", "clave"),
    ("passphrase", "phrase de passe", "frase de contraseña"),
    ("off: stored in clear", "désactivé : stocké en clair", "desactivado: guardado sin cifrar"),
    ("none yet", "aucun pour l'instant", "ninguno todavía"),
    ("not yet", "pas encore", "todavía no"),
    ("{} ago", "il y a {}", "hace {}"),
    ("never", "jamais", "nunca"),
    ("off", "désactivée", "desactivada"),
    ("to {}", "vers {}", "hacia {}"),
    ("failing", "en échec", "con errores"),
    ("with the server", "avec le serveur", "con el servidor"),
    (
        "Data is stored in <code>{}</code>.",
        "Les données sont stockées dans <code>{}</code>.",
        "Los datos se guardan en <code>{}</code>.",
    ),
    (
        "Data is stored in <code>{}</code> (the others in <code>apps/</code>).",
        "Les données sont stockées dans <code>{}</code> (celles des autres applications dans <code>apps/</code>).",
        "Los datos se guardan en <code>{}</code> (los de las demás aplicaciones en <code>apps/</code>).",
    ),
    // Connect
    ("Connect", "Connecter une application", "Conectar una aplicación"),
    (
        "The token an application sends picks its database: it reads and writes only its own.",
        "Le jeton qu'envoie une application choisit sa base : elle ne lit et n'écrit que la sienne.",
        "El token que envía una aplicación elige su base: solo lee y escribe la suya.",
    ),
    ("a token of this application", "un jeton de cette application", "un token de esta aplicación"),
    ("Connect a website", "Brancher un site", "Conectar un sitio"),
    (
        "Any language, or a page's JavaScript, sends SQL with <code>?</code> parameters to <code>POST /v1/sql</code> and gets rows as JSON. In a page, use a <b>read-only</b> token.",
        "N'importe quel langage, ou le JavaScript d'une page, envoie du SQL avec des paramètres <code>?</code> à <code>POST /v1/sql</code> et reçoit des lignes en JSON. Dans une page, utilisez un jeton <b>en lecture seule</b>.",
        "Cualquier lenguaje, o el JavaScript de una página, envía SQL con parámetros <code>?</code> a <code>POST /v1/sql</code> y recibe filas en JSON. En una página, usa un token <b>de solo lectura</b>.",
    ),
    (
        r#"No website yet: add yours in <a href="{}/settings">Settings</a>."#,
        r#"Aucun site pour l'instant : ajoutez le vôtre dans les <a href="{}/settings">Réglages</a>."#,
        r#"Ningún sitio todavía: añade el tuyo en <a href="{}/settings">Ajustes</a>."#,
    ),
    ("Websites allowed: {}.", "Sites autorisés : {}.", "Sitios permitidos: {}."),
    (
        "New token of {}: copy it now, it is not shown again",
        "Nouveau jeton de {} : copiez-le maintenant, il ne sera plus affiché",
        "Nuevo token de {}: cópialo ahora, no se volverá a mostrar",
    ),
    (
        "(the server keeps only its digest).",
        "(le serveur n'en garde que l'empreinte).",
        "(el servidor solo guarda su huella).",
    ),
    ("Connect the application with it:", "Connectez l'application avec :", "Conecta la aplicación con él:"),
    // Tables
    ("Table", "Table", "Tabla"),
    ("Columns", "Colonnes", "Columnas"),
    ("Browse {}", "Parcourir {}", "Ver {}"),
    ("No rows.", "Aucune ligne.", "Ninguna fila."),
    ("Previous", "Précédente", "Anterior"),
    ("Next", "Suivante", "Siguiente"),
    ("Page {}", "Page {}", "Página {}"),
    // Query
    ("Run", "Exécuter", "Ejecutar"),
    (
        "SQL: SELECT with joins, subqueries, GROUP BY, UNION, functions; INSERT (ON CONFLICT, RETURNING), UPDATE, DELETE; CREATE TABLE, CREATE INDEX, ALTER TABLE ADD COLUMN, DROP. Several statements separated by <code>;</code> run in one transaction. Changes ask for a confirmation first. Websites send the same SQL to <code>POST /v1/sql</code>.",
        "SQL : SELECT avec jointures, sous-requêtes, GROUP BY, UNION, fonctions ; INSERT (ON CONFLICT, RETURNING), UPDATE, DELETE ; CREATE TABLE, CREATE INDEX, ALTER TABLE ADD COLUMN, DROP. Plusieurs instructions séparées par <code>;</code> forment une seule transaction. Les modifications demandent d'abord une confirmation. Les sites envoient le même SQL à <code>POST /v1/sql</code>.",
        "SQL: SELECT con joins, subconsultas, GROUP BY, UNION, funciones; INSERT (ON CONFLICT, RETURNING), UPDATE, DELETE; CREATE TABLE, CREATE INDEX, ALTER TABLE ADD COLUMN, DROP. Varias instrucciones separadas por <code>;</code> forman una sola transacción. Los cambios piden una confirmación antes. Los sitios envían el mismo SQL a <code>POST /v1/sql</code>.",
    ),
    (
        "This changes data of {}: {} row(s) would be affected.",
        "Ceci modifie des données de {} : {} ligne(s) seraient touchées.",
        "Esto cambia datos de {}: {} fila(s) se verían afectadas.",
    ),
    ("Confirm and run", "Confirmer et exécuter", "Confirmar y ejecutar"),
    ("{} row(s).", "{} ligne(s).", "{} fila(s)."),
    ("Done: {} row(s) affected.", "Fait : {} ligne(s) touchée(s).", "Hecho: {} fila(s) afectada(s)."),
    // Branches
    (
        "A branch is an instant copy of the database: it shares the data and stores only its own changes.",
        "Une branche est une copie instantanée de la base : elle partage les données et ne stocke que ses propres changements.",
        "Una rama es una copia instantánea de la base: comparte los datos y solo guarda sus propios cambios.",
    ),
    ("Branch", "Branche", "Rama"),
    ("From", "Depuis", "Desde"),
    ("Own data", "Données propres", "Datos propios"),
    ("Reset", "Réinitialiser", "Restablecer"),
    ("Delete", "Supprimer", "Borrar"),
    ("No branch yet.", "Aucune branche pour l'instant.", "Ninguna rama todavía."),
    ("New branch", "Nouvelle branche", "Nueva rama"),
    ("Branch {} created.", "Branche {} créée.", "Rama {} creada."),
    ("Branch {} deleted.", "Branche {} supprimée.", "Rama {} borrada."),
    (
        "Branch {} reset to its parent.",
        "Branche {} remise à l'état de son parent.",
        "Rama {} restablecida al estado de su padre.",
    ),
    ("There is no branch {}.", "Il n'y a pas de branche {}.", "No hay ninguna rama {}."),
    ("Unknown action.", "Action inconnue.", "Acción desconocida."),
    // Storage
    ("File", "Fichier", "Archivo"),
    ("Data", "Données", "Datos"),
    ("Free space in the file", "Espace libre dans le fichier", "Espacio libre en el archivo"),
    ("Pages", "Pages", "Páginas"),
    ("{} of {}", "{} de {}", "{} de {}"),
    ("Write-ahead log", "Journal d'écriture", "Registro de escritura"),
    ("Free disk", "Disque libre", "Disco libre"),
    ("unknown", "inconnu", "desconocido"),
    ("Cache", "Cache", "Caché"),
    ("Cache hits", "Succès du cache", "Aciertos de caché"),
    ("Last commit", "Dernier commit", "Último commit"),
    ("Backup target", "Destination des sauvegardes", "Destino de las copias"),
    ("Last backup", "Dernière sauvegarde", "Última copia"),
    ("failed: {}", "échec : {}", "error: {}"),
    ("Maintenance", "Entretien", "Mantenimiento"),
    ("Checkpoint now", "Checkpoint maintenant", "Checkpoint ahora"),
    ("Back up now", "Sauvegarder maintenant", "Copiar ahora"),
    ("Check integrity", "Vérifier l'intégrité", "Comprobar la integridad"),
    ("Compact", "Compacter", "Compactar"),
    ("Checkpoint at {}.", "Checkpoint à {}.", "Checkpoint en {}."),
    ("Backup done.", "Sauvegarde faite.", "Copia hecha."),
    (
        "Every page checksum is right.",
        "Toutes les sommes de contrôle des pages sont justes.",
        "Todas las sumas de control de las páginas son correctas.",
    ),
    (
        "Damaged pages were found and repaired.",
        "Des pages abîmées ont été trouvées et réparées.",
        "Se encontraron y repararon páginas dañadas.",
    ),
    ("Compaction done.", "Compactage fait.", "Compactación hecha."),
    (
        "Busy: compaction will run later.",
        "Occupé : le compactage se fera plus tard.",
        "Ocupado: la compactación se hará más tarde.",
    ),
    // Tokens
    (
        "A token opens this application's database, and only it. The server keeps a SHA-256 digest of each token, never the token: a lost token cannot be shown again, make a new one.",
        "Un jeton ouvre la base de cette application, et elle seule. Le serveur garde une empreinte SHA-256 de chaque jeton, jamais le jeton : un jeton perdu ne peut pas être réaffiché, créez-en un nouveau.",
        "Un token abre la base de esta aplicación, y solo esa. El servidor guarda una huella SHA-256 de cada token, nunca el token: un token perdido no se puede volver a mostrar, crea uno nuevo.",
    ),
    ("Token", "Jeton", "Token"),
    ("Rights", "Droits", "Permisos"),
    ("read and write", "lecture et écriture", "lectura y escritura"),
    ("read only", "lecture seule", "solo lectura"),
    ("set on the server", "défini sur le serveur", "definido en el servidor"),
    ("change it in the environment", "à changer dans l'environnement", "se cambia en el entorno"),
    ("Revoke", "Révoquer", "Revocar"),
    (
        "No token: no application can connect.",
        "Aucun jeton : aucune application ne peut se connecter.",
        "Ningún token: ninguna aplicación puede conectarse.",
    ),
    ("New token (read and write)", "Nouveau jeton (lecture et écriture)", "Nuevo token (lectura y escritura)"),
    ("New read-only token", "Nouveau jeton en lecture seule", "Nuevo token de solo lectura"),
    (
        "A token in a web page is public: anyone can read it in the page's code. Give pages a <b>read-only</b> token, and keep read-and-write tokens on servers.",
        "Un jeton dans une page web est public : n'importe qui peut le lire dans le code de la page. Donnez aux pages un jeton <b>en lecture seule</b>, et gardez les jetons lecture-écriture sur des serveurs.",
        "Un token en una página web es público: cualquiera puede leerlo en el código de la página. Da a las páginas un token <b>de solo lectura</b> y guarda los de lectura y escritura en servidores.",
    ),
    (
        "Token nuage_{}… revoked: it opens nothing any more.",
        "Jeton nuage_{}… révoqué : il n'ouvre plus rien.",
        "Token nuage_{}… revocado: ya no abre nada.",
    ),
    ("There is no such token.", "Ce jeton n'existe pas.", "Ese token no existe."),
    // Settings
    ("what this application is", "à quoi sert cette application", "para qué sirve esta aplicación"),
    ("Save", "Enregistrer", "Guardar"),
    ("Description saved.", "Description enregistrée.", "Descripción guardada."),
    ("Websites", "Sites web", "Sitios web"),
    (
        "The pages of these websites may call this application from the browser (CORS): one origin per line, scheme and host (<code>https://site.fr</code>, <code>http://localhost:3000</code>), or <code>*</code> for any. Servers, and apps outside a browser, need none.",
        "Les pages de ces sites peuvent appeler cette application depuis le navigateur (CORS) : une origine par ligne, schéma et hôte (<code>https://site.fr</code>, <code>http://localhost:3000</code>), ou <code>*</code> pour tous. Les serveurs, et les applications hors navigateur, n'en ont pas besoin.",
        "Las páginas de estos sitios pueden llamar a esta aplicación desde el navegador (CORS): un origen por línea, esquema y host (<code>https://sitio.es</code>, <code>http://localhost:3000</code>), o <code>*</code> para todos. Los servidores, y las aplicaciones fuera de un navegador, no lo necesitan.",
    ),
    ("Save the websites", "Enregistrer les sites", "Guardar los sitios"),
    (
        "`{}` is not an origin: write `https://site.fr` (scheme and host, no path), or `*`",
        "`{}` n'est pas une origine : écrivez `https://site.fr` (schéma et hôte, sans chemin), ou `*`",
        "`{}` no es un origen: escribe `https://sitio.es` (esquema y host, sin ruta), o `*`",
    ),
    (
        "No website: pages cannot call this application.",
        "Aucun site : les pages ne peuvent pas appeler cette application.",
        "Ningún sitio: las páginas no pueden llamar a esta aplicación.",
    ),
    (
        "Pages of {} may call this application.",
        "Les pages de {} peuvent appeler cette application.",
        "Las páginas de {} pueden llamar a esta aplicación.",
    ),
    (
        "The first application is the server's own database: it cannot be deleted.",
        "La première application est la base du serveur lui-même : elle ne peut pas être supprimée.",
        "La primera aplicación es la base del propio servidor: no se puede borrar.",
    ),
    (
        "Deleting <b>{}</b> removes its tokens, its data and its backups. It cannot be undone.",
        "Supprimer <b>{}</b> efface ses jetons, ses données et ses sauvegardes. C'est irréversible.",
        "Borrar <b>{}</b> elimina sus tokens, sus datos y sus copias. No se puede deshacer.",
    ),
    (
        "Type <code>{}</code> to confirm",
        "Tapez <code>{}</code> pour confirmer",
        "Escribe <code>{}</code> para confirmar",
    ),
    ("Delete {}", "Supprimer {}", "Borrar {}"),
    (
        "Type {} to confirm: nothing was deleted.",
        "Tapez {} pour confirmer : rien n'a été supprimé.",
        "Escribe {} para confirmar: no se ha borrado nada.",
    ),
    (
        "This application could not be opened at startup (see the server's events).",
        "Cette application n'a pas pu être ouverte au démarrage (voir les événements du serveur).",
        "Esta aplicación no se pudo abrir al arrancar (mira los eventos del servidor).",
    ),
    // Server
    ("Version", "Version", "Versión"),
    ("Uptime", "Disponibilité", "Tiempo activo"),
    ("Connections", "Connexions", "Conexiones"),
    ("{} active, {} idle", "{} actives, {} inactives", "{} activas, {} inactivas"),
    ("Transactions open", "Transactions ouvertes", "Transacciones abiertas"),
    (
        "off for some data: set NUAGE_ENCRYPTION_KEY",
        "désactivé pour certaines données : définissez NUAGE_ENCRYPTION_KEY",
        "desactivado para algunos datos: define NUAGE_ENCRYPTION_KEY",
    ),
    ("Events", "Événements", "Eventos"),
    ("Memory", "Mémoire", "Memoria"),
    ("Swap", "Swap", "Swap"),
    ("none", "aucun", "ninguno"),
    ("{} free of {}", "{} libres sur {}", "{} libres de {}"),
    // The Settings page.
    (
        "What you save here applies at once and is kept with the data. It wins over the environment (the settings file, the host's variables); each line says where its value comes from, and one button gives it back.",
        "Ce que vous enregistrez ici s'applique tout de suite et se conserve avec les données. Cela l'emporte sur l'environnement (le fichier de réglages, les variables de l'hébergeur) ; chaque ligne dit d'où vient sa valeur, et un bouton la remet à la valeur de l'environnement.",
        "Lo que guardas aquí se aplica al momento y se conserva con los datos. Tiene prioridad sobre el entorno (el archivo de ajustes, las variables del alojamiento); cada línea dice de dónde viene su valor, y un botón vuelve a poner el valor del entorno.",
    ),
    (
        "The settings of this server are those of its environment (<code>NUAGE_SETTINGS=environment</code>): they are shown here, and change there.",
        "Les réglages de ce serveur sont ceux de son environnement (<code>NUAGE_SETTINGS=environment</code>) : ils s'affichent ici et se changent là-bas.",
        "Los ajustes de este servidor son los de su entorno (<code>NUAGE_SETTINGS=environment</code>): se muestran aquí y se cambian allí.",
    ),
    (
        "Saved here: it wins over the environment (<code>{}={}</code>).",
        "Enregistré ici : il l'emporte sur l'environnement (<code>{}={}</code>).",
        "Guardado aquí: tiene prioridad sobre el entorno (<code>{}={}</code>).",
    ),
    (
        "Saved here (<code>{}</code> is not set in the environment).",
        "Enregistré ici (<code>{}</code> n'est pas réglé dans l'environnement).",
        "Guardado aquí (<code>{}</code> no está definido en el entorno).",
    ),
    (
        "From the environment (<code>{}={}</code>).",
        "Vient de l'environnement (<code>{}={}</code>).",
        "Viene del entorno (<code>{}={}</code>).",
    ),
    (
        "Default (<code>{}</code> is not set).",
        "Valeur par défaut (<code>{}</code> n'est pas réglé).",
        "Valor por defecto (<code>{}</code> no está definido).",
    ),
    ("Back to the environment's value", "Revenir à la valeur de l'environnement", "Volver al valor del entorno"),
    ("Back to the default", "Revenir à la valeur par défaut", "Volver al valor por defecto"),
    (
        "This server may use {}: {} used, {} free.",
        "Ce serveur peut utiliser {} : {} utilisés, {} libres.",
        "Este servidor puede usar {}: {} usados, {} libres.",
    ),
    (
        "This system does not tell Nuage DB its memory: say how much it may count on.",
        "Ce système ne dit pas sa mémoire à Nuage DB : indiquez sur combien il peut compter.",
        "Este sistema no le dice su memoria a Nuage DB: indica con cuánta puede contar.",
    ),
    (
        "Memory Nuage DB may count on",
        "Mémoire sur laquelle Nuage DB peut compter",
        "Memoria con la que Nuage DB puede contar",
    ),
    (
        "<code>auto</code>: what the system reports ({}), or <code>NUAGE_MEMORY_MB</code> when it is set.",
        "<code>auto</code> : ce que le système annonce ({}), ou <code>NUAGE_MEMORY_MB</code> s'il est réglé.",
        "<code>auto</code>: lo que indica el sistema ({}), o <code>NUAGE_MEMORY_MB</code> si está definido.",
    ),
    (
        "Page caches, all the applications together",
        "Caches de pages, toutes les applications ensemble",
        "Cachés de páginas, todas las aplicaciones juntas",
    ),
    (
        "<code>auto</code> follows the memory: a quarter of it at first, up to six tenths while there is room, less when it gets tight. An amount is a maximum. Now: {} (up to {}).",
        "<code>auto</code> suit la mémoire : un quart au départ, jusqu'à six dixièmes tant qu'il y a de la place, moins quand elle se resserre. Une quantité est un maximum. En ce moment : {} (jusqu'à {}).",
        "<code>auto</code> sigue la memoria: un cuarto al principio, hasta seis décimos mientras haya sitio, menos cuando escasea. Una cantidad es un máximo. Ahora: {} (hasta {}).",
    ),
    (
        "Key-value store of each application",
        "Stockage clé-valeur de chaque application",
        "Almacén clave-valor de cada aplicación",
    ),
    (
        "<code>auto</code>: a sixteenth of the memory each, an eighth for all of them together. Lowering it drops the least recently used keys at once. Now: {} each.",
        "<code>auto</code> : un seizième de la mémoire chacun, un huitième pour tous ensemble. Le baisser retire tout de suite les clés les moins récemment utilisées. En ce moment : {} chacun.",
        "<code>auto</code>: un dieciseisavo de la memoria cada uno, un octavo para todos juntos. Bajarlo quita al momento las claves usadas hace más tiempo. Ahora: {} cada uno.",
    ),
    ("Save the memory", "Enregistrer la mémoire", "Guardar la memoria"),
    ("Memory by application", "Mémoire par application", "Memoria por aplicación"),
    (
        "Each application shares the page caches of the server by use (<code>auto</code>), or has an amount set apart for it alone.",
        "Chaque application partage les caches de pages du serveur selon l'usage (<code>auto</code>), ou dispose d'une part réservée pour elle seule.",
        "Cada aplicación comparte las cachés de páginas del servidor según el uso (<code>auto</code>), o tiene una parte reservada solo para ella.",
    ),
    ("Its cache holds", "Son cache contient", "Su caché contiene"),
    ("may hold", "peut contenir", "puede contener"),
    ("Page cache", "Cache de pages", "Caché de páginas"),
    ("Key-value store", "Stockage clé-valeur", "Almacén clave-valor"),
    (
        "Amounts are written <code>auto</code>, <code>512 MiB</code> or <code>2 GiB</code> (a number alone is in MiB).",
        "Les quantités s'écrivent <code>auto</code>, <code>512 Mio</code> ou <code>2 Gio</code> (<code>2 Go</code> convient aussi ; un nombre seul est en Mio).",
        "Las cantidades se escriben <code>auto</code>, <code>512 MiB</code> o <code>2 GiB</code> (un número solo va en MiB).",
    ),
    (
        "On a phone, each application's Settings tab has the same fields, one under the other.",
        "Sur un téléphone, l'onglet Réglages de chaque application a les mêmes champs, l'un sous l'autre.",
        "En un teléfono, la pestaña Ajustes de cada aplicación tiene los mismos campos, uno debajo del otro.",
    ),
    (
        "Set apart: {} of the {} the caches may take (every application has an amount of its own). When memory runs short, what is set apart shrinks with the rest, and comes back.",
        "Réservé : {} sur les {} que les caches peuvent prendre (chaque application a son propre montant). Quand la mémoire manque, ce qui est réservé diminue avec le reste, puis revient.",
        "Reservado: {} de los {} que las cachés pueden ocupar (cada aplicación tiene su propia cantidad). Cuando falta memoria, lo reservado se reduce con el resto, y luego vuelve.",
    ),
    (
        "Set apart: {} of the {} the caches may take; the others share the rest by use. When memory runs short, what is set apart shrinks with the rest, and comes back.",
        "Réservé : {} sur les {} que les caches peuvent prendre ; les autres se partagent le reste selon l'usage. Quand la mémoire manque, ce qui est réservé diminue avec le reste, puis revient.",
        "Reservado: {} de los {} que las cachés pueden ocupar; las demás aplicaciones comparten el resto según el uso. Cuando falta memoria, lo reservado se reduce con el resto, y luego vuelve.",
    ),
    (
        "Memory is short: what is set apart is cut to {} of {} for now.",
        "La mémoire manque : ce qui est réservé est réduit à {} sur {} pour le moment.",
        "Falta memoria: lo reservado se reduce a {} de {} por ahora.",
    ),
    ("Limits", "Limites", "Límites"),
    ("Memory of one statement", "Mémoire d'une instruction", "Memoria de una sentencia"),
    (
        "What one SQL statement may hold at once (the rows it sorts, joins or returns); past it, the statement fails with a clear error instead of filling the memory. <code>auto</code>: an eighth of the memory.",
        "Ce qu'une instruction SQL peut garder en mémoire à la fois (les lignes qu'elle trie, joint ou renvoie) ; au-delà, elle échoue avec une erreur claire au lieu de remplir la mémoire. <code>auto</code> : un huitième de la mémoire.",
        "Lo que una sentencia SQL puede retener a la vez (las filas que ordena, une o devuelve); más allá, falla con un error claro en lugar de llenar la memoria. <code>auto</code>: un octavo de la memoria.",
    ),
    ("Seconds a statement may run", "Secondes qu'une instruction peut durer", "Segundos que puede durar una sentencia"),
    (
        "Past it, the statement stops with an error and its changes are undone. <code>0</code>: no limit.",
        "Au-delà, l'instruction s'arrête avec une erreur et ses changements sont annulés. <code>0</code> : pas de limite.",
        "Más allá, la sentencia se detiene con un error y sus cambios se deshacen. <code>0</code>: sin límite.",
    ),
    ("PostgreSQL connections at once", "Connexions PostgreSQL simultanées", "Conexiones PostgreSQL simultáneas"),
    (
        "Each has a thread; the next ones are refused as PostgreSQL does.",
        "Chacune a un thread ; les suivantes sont refusées, comme le fait PostgreSQL.",
        "Cada una tiene un hilo; las siguientes se rechazan, como hace PostgreSQL.",
    ),
    ("Save the limits", "Enregistrer les limites", "Guardar los límites"),
    ("Visibility", "Visibilité", "Visibilidad"),
    (
        "Public status: <code>/api/status</code> and the dashboard show a public subset without signing in",
        "État public : <code>/api/status</code> et le tableau de bord montrent une partie publique sans connexion",
        "Estado público: <code>/api/status</code> y el panel muestran una parte pública sin iniciar sesión",
    ),
    ("Save the visibility", "Enregistrer la visibilité", "Guardar la visibilidad"),
    ("Set when the server starts", "Réglés au démarrage du serveur", "Definidos al arrancar el servidor"),
    (
        "The server reads these before it opens its data, or they are what it listens on and what opens it: a wrong value saved from here could lock you out. They change in the environment, then the server restarts.",
        "Le serveur les lit avant d'ouvrir ses données, ou bien ce sont ses ports et ce qui lui donne accès : une mauvaise valeur enregistrée d'ici pourrait vous en fermer la porte. Ils se changent dans l'environnement, puis le serveur redémarre.",
        "El servidor los lee antes de abrir sus datos, o son sus puertos y lo que da acceso a él: un valor erróneo guardado desde aquí podría dejarte fuera. Se cambian en el entorno, y luego el servidor se reinicia.",
    ),
    (
        "On this computer: edit <code>{}</code>, then restart the service.",
        "Sur cet ordinateur : modifiez <code>{}</code>, puis redémarrez le service.",
        "En este equipo: edita <code>{}</code> y reinicia el servicio.",
    ),
    (
        "On Heroku: <code>heroku config:set NAME=value</code> (the server restarts).",
        "Sur Heroku : <code>heroku config:set NOM=valeur</code> (le serveur redémarre).",
        "En Heroku: <code>heroku config:set NOMBRE=valor</code> (el servidor se reinicia).",
    ),
    (
        "Change the variables of the environment (Docker: <code>.env</code>), then restart the server.",
        "Changez les variables de l'environnement (Docker : <code>.env</code>), puis redémarrez le serveur.",
        "Cambia las variables del entorno (Docker: <code>.env</code>) y reinicia el servidor.",
    ),
    ("Variable", "Variable", "Variable"),
    ("Value", "Valeur", "Valor"),
    ("set (a secret: not shown)", "réglé (un secret : non affiché)", "definido (un secreto: no se muestra)"),
    ("Nothing changed.", "Rien n'a changé.", "Nada ha cambiado."),
    (
        "{} is given back: the value in force is shown below.",
        "{} revient à la valeur de l'environnement (ou par défaut) : la valeur en vigueur s'affiche ci-dessous.",
        "{} vuelve al valor del entorno (o al de por defecto): el valor en vigor se muestra abajo.",
    ),
    (
        "Settings saved: in force now.",
        "Réglages enregistrés : en vigueur dès maintenant.",
        "Ajustes guardados: en vigor desde ahora.",
    ),
    (
        "This application shares the server's memory with the others (<code>auto</code>), or has an amount of its own (<code>64 MiB</code>, <code>2 GiB</code>). Its cache holds {} of {} now.",
        "Cette application partage la mémoire du serveur avec les autres (<code>auto</code>), ou dispose de sa propre part (<code>64 Mio</code>, <code>2 Gio</code>). Son cache contient {} sur {} en ce moment.",
        "Esta aplicación comparte la memoria del servidor con las demás (<code>auto</code>), o tiene una parte propia (<code>64 MiB</code>, <code>2 GiB</code>). Su caché contiene {} de {} ahora.",
    ),
    ("All the memory settings", "Tous les réglages de mémoire", "Todos los ajustes de memoria"),
    (
        "Memory of {} saved: in force now.",
        "Mémoire de {} enregistrée : en vigueur dès maintenant.",
        "Memoria de {} guardada: en vigor desde ahora.",
    ),
    // Why a setting is refused.
    (
        "`{}`: {} MiB is more than any server has: a number alone is in MiB",
        "`{}` : {} Mio, c'est plus que n'importe quel serveur : un nombre seul est en Mio",
        "`{}`: {} MiB es más de lo que tiene cualquier servidor: un número solo va en MiB",
    ),
    (
        "`{}`: {} MiB is too little: {} MiB at least, or `auto`",
        "`{}` : {} Mio, c'est trop peu : {} Mio au moins, ou `auto`",
        "`{}`: {} MiB es demasiado poco: {} MiB como mínimo, o `auto`",
    ),
    (
        "`{}`: `{}` is not an amount of memory: write `auto`, `512 MiB` or `2 GiB` (a number alone is in MiB)",
        "`{}` : `{}` n'est pas une quantité de mémoire : écrivez `auto`, `512 Mio` ou `2 Gio` (un nombre seul est en Mio)",
        "`{}`: `{}` no es una cantidad de memoria: escribe `auto`, `512 MiB` o `2 GiB` (un número solo va en MiB)",
    ),
    ("`{}`: {} is not between {} and {}", "`{}` : {} n'est pas entre {} et {}", "`{}`: {} no está entre {} y {}"),
    ("`{}`: `{}` is not a whole number", "`{}` : `{}` n'est pas un nombre entier", "`{}`: `{}` no es un número entero"),
    (
        "`{}`: `{}` is not `true` or `false`",
        "`{}` : `{}` n'est ni `true` ni `false`",
        "`{}`: `{}` no es `true` ni `false`",
    ),
    (
        "there is no setting `{}`: the settings are {}",
        "il n'y a pas de réglage `{}` : les réglages sont {}",
        "no hay ningún ajuste `{}`: los ajustes son {}",
    ),
    (
        "the settings of this server are those of its environment (NUAGE_SETTINGS=environment): change them there",
        "les réglages de ce serveur sont ceux de son environnement (NUAGE_SETTINGS=environment) : changez-les là-bas",
        "los ajustes de este servidor son los de su entorno (NUAGE_SETTINGS=environment): cámbialos allí",
    ),
    (
        "`memory.total_mb`: {} MiB is more than this server has ({} MiB): it would be killed for using it",
        "`memory.total_mb` : {} Mio, c'est plus que ce serveur n'en a ({} Mio) : il serait tué en les utilisant",
        "`memory.total_mb`: {} MiB es más de lo que tiene este servidor ({} MiB): el sistema lo detendría si la usara",
    ),
    (
        "`memory.cache_mb`: {} MiB is less than what is set apart for single databases ({} MiB, and {} MiB for the others to share): {} MiB at least, or lower their amounts first",
        "`memory.cache_mb` : {} Mio, c'est moins que ce qui est réservé à des bases ({} Mio, plus {} Mio que les autres se partagent) : {} Mio au moins, ou baissez d'abord leurs parts",
        "`memory.cache_mb`: {} MiB es menos de lo reservado para bases concretas ({} MiB, más {} MiB que comparten las demás): {} MiB como mínimo, o baja antes sus partes",
    ),
    (
        "a page cache needs {} MiB at least",
        "un cache de pages a besoin d'au moins {} Mio",
        "una caché de páginas necesita al menos {} MiB",
    ),
    (
        "{} MiB for `{}` is more than is left: the page caches of this server may take {} MiB in all, {} MiB of it are set apart for other databases and {} MiB stay for those that share: {} MiB at most",
        "{} Mio pour `{}`, c'est plus que ce qui reste : les caches de pages de ce serveur peuvent prendre {} Mio en tout, dont {} Mio réservés à d'autres bases et {} Mio pour celles qui partagent : {} Mio au plus",
        "{} MiB para `{}` es más de lo que queda: las cachés de páginas de este servidor pueden ocupar {} MiB en total, de los cuales {} MiB están reservados para otras bases y {} MiB quedan para las que comparten: {} MiB como máximo",
    ),
    (
        "a key-value store needs {} MiB at least",
        "un stockage clé-valeur a besoin d'au moins {} Mio",
        "un almacén clave-valor necesita al menos {} MiB",
    ),
    (
        "`memory.total_mb`: this server already uses {} MiB without its caches: {} MiB at least",
        "`memory.total_mb` : ce serveur utilise déjà {} Mio sans ses caches : {} Mio au moins",
        "`memory.total_mb`: este servidor ya usa {} MiB sin sus cachés: {} MiB como mínimo",
    ),
    (
        "{}: the key-value stores would take {} MiB in all: more than a quarter of the {} MiB this server counts on ({} MiB at most)",
        "{} : les stockages clé-valeur prendraient {} Mio en tout : plus du quart des {} Mio sur lesquels ce serveur compte ({} Mio au plus)",
        "{}: los almacenes clave-valor ocuparían {} MiB en total: más de la cuarta parte de los {} MiB con los que cuenta este servidor ({} MiB como máximo)",
    ),
    (
        "{}: {} MiB would be set by hand ({} MiB for the page caches, {} MiB for the key-value stores): more than three quarters of the {} MiB this server counts on ({} MiB at most)",
        "{} : {} Mio seraient fixés à la main ({} Mio pour les caches de pages, {} Mio pour les stockages clé-valeur) : plus des trois quarts des {} Mio sur lesquels ce serveur compte ({} Mio au plus)",
        "{}: se fijarían a mano {} MiB ({} MiB para las cachés de páginas, {} MiB para los almacenes clave-valor): más de tres cuartos de los {} MiB con los que cuenta este servidor ({} MiB como máximo)",
    ),
    (
        "`memory.total_mb`: {} MiB is too little for what is set by hand ({} MiB for the page caches, {} MiB for the key-value stores): {} MiB at least, or lower those amounts first",
        "`memory.total_mb` : {} Mio, c'est trop peu pour ce qui est fixé à la main ({} Mio pour les caches de pages, {} Mio pour les stockages clé-valeur) : {} Mio au moins, ou baissez d'abord ces montants",
        "`memory.total_mb`: {} MiB es demasiado poco para lo fijado a mano ({} MiB para las cachés de páginas, {} MiB para los almacenes clave-valor): {} MiB como mínimo, o baja antes esas cantidades",
    ),
    (
        "`postgres.connections`: {} is more than this server can serve at once (each connection has a thread): {} at most",
        "`postgres.connections` : {}, c'est plus que ce serveur ne peut en servir à la fois (chaque connexion a son fil d'exécution) : {} au plus",
        "`postgres.connections`: {} es más de lo que este servidor puede atender a la vez (cada conexión tiene su hilo): {} como máximo",
    ),
    (
        "The environment's figure is lower: {} is in force (a saved memory only lowers it).",
        "Le chiffre de l'environnement est plus bas : {} est en vigueur (une mémoire enregistrée ne peut que la baisser).",
        "La cifra del entorno es más baja: {} está en vigor (una memoria guardada solo puede bajarla).",
    ),
    (
        "`statement.memory_mb`: {} MiB is more than half of the {} MiB this server counts on: {} MiB at most",
        "`statement.memory_mb` : {} Mio, c'est plus de la moitié des {} Mio sur lesquels ce serveur compte : {} Mio au plus",
        "`statement.memory_mb`: {} MiB es más de la mitad de los {} MiB con los que cuenta este servidor: {} MiB como máximo",
    ),
    ("Saved, and ignored", "Enregistrés mais ignorés", "Guardados pero ignorados"),
    ("Remove", "Retirer", "Quitar"),
    // The second copy of the backups.
    (
        "the second copy is a folder (`dir`) or S3-compatible storage (`s3`)",
        "la copie de secours est un dossier (`dir`) ou un stockage compatible S3 (`s3`)",
        "la segunda copia es una carpeta (`dir`) o un almacenamiento compatible con S3 (`s3`)",
    ),
    (
        "`{}` is not the full path of a folder: write it from the root (`/mnt/backup/nuage`, `E:\\Backup\\nuage`)",
        "`{}` n'est pas le chemin complet d'un dossier : écrivez-le depuis la racine (`/mnt/backup/nuage`, `E:\\Backup\\nuage`)",
        "`{}` no es la ruta completa de una carpeta: escríbela desde la raíz (`/mnt/backup/nuage`, `E:\\Backup\\nuage`)",
    ),
    (
        "the folder that would hold `{}` does not exist: a disk that is not plugged in, or a mistyped path (only the last folder is made for you)",
        "le dossier qui contiendrait `{}` n'existe pas : un disque non branché, ou un chemin mal tapé (seul le dernier dossier est créé pour vous)",
        "la carpeta que contendría `{}` no existe: un disco sin conectar, o una ruta mal escrita (solo se crea la última carpeta)",
    ),
    (
        "`{}` is inside the data directory: a copy on the disk of the data is lost with it, choose another disk",
        "`{}` est dans le dossier des données : une copie sur le disque des données se perd avec lui, choisissez un autre disque",
        "`{}` está dentro del directorio de datos: una copia en el disco de los datos se pierde con él, elige otro disco",
    ),
    (
        "S3-compatible storage needs `{}`",
        "un stockage compatible S3 a besoin de `{}`",
        "un almacenamiento compatible con S3 necesita `{}`",
    ),
    (
        "`{}` is where the backups already go: a second copy is somewhere else",
        "`{}` est déjà la destination des sauvegardes : une copie de secours va ailleurs",
        "`{}` ya es el destino de las copias: una segunda copia va a otro sitio",
    ),
    ("nothing could be written to {}: {}", "rien n'a pu être écrit dans {} : {}", "no se pudo escribir nada en {}: {}"),
    (
        "on this host the disk is erased at each restart, and the server could not find this place again by itself: name the destination of the backups in the environment (NUAGE_BACKUP_S3_*); a second copy can be added here once there is one",
        "sur cet hébergement le disque est effacé à chaque redémarrage, et le serveur ne retrouverait pas cet endroit tout seul : indiquez la destination des sauvegardes dans l'environnement (NUAGE_BACKUP_S3_*) ; une copie de secours pourra s'ajouter ici ensuite",
        "en este alojamiento el disco se borra en cada reinicio, y el servidor no volvería a encontrar este sitio por sí solo: indica el destino de las copias en el entorno (NUAGE_BACKUP_S3_*); después se podrá añadir aquí una segunda copia",
    ),
    (
        "this build of Nuage DB has no `s3` feature: the second copy is a folder",
        "cette version de Nuage DB n'a pas la fonction `s3` : la copie de secours est un dossier",
        "esta compilación de Nuage DB no tiene la función `s3`: la segunda copia es una carpeta",
    ),
    (
        "Not yet in the backup: the backup failed (the log says why). A restart of the server now would bring the earlier value back.",
        "Pas encore dans la sauvegarde : elle a échoué (le journal dit pourquoi). Un redémarrage du serveur maintenant ferait revenir l'ancienne valeur.",
        "Aún no está en la copia: la copia falló (el registro dice por qué). Un reinicio del servidor ahora devolvería el valor anterior.",
    ),
    (
        "the saved settings were written by another version of Nuage DB and cannot be read by this one: nothing is saved over them (start the version that wrote them, or NUAGE_SETTINGS=environment)",
        "les réglages enregistrés ont été écrits par une autre version de Nuage DB et celle-ci ne sait pas les lire : rien n'est enregistré par-dessus (démarrez la version qui les a écrits, ou NUAGE_SETTINGS=environment)",
        "los ajustes guardados los escribió otra versión de Nuage DB y esta no sabe leerlos: no se guarda nada encima (arranca la versión que los escribió, o NUAGE_SETTINGS=environment)",
    ),
    (
        "application {}: page cache {}, key-value store {}",
        "application {} : cache de pages {}, stockage clé-valeur {}",
        "aplicación {}: caché de páginas {}, almacén clave-valor {}",
    ),
    (
        "The memory was lowered since: what is set apart has {} of the {} asked, in proportion.",
        "La mémoire a été réduite depuis : ce qui est réservé a {} sur les {} demandés, en proportion.",
        "La memoria se ha reducido desde entonces: lo reservado tiene {} de los {} pedidos, en proporción.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    #[test]
    fn languages_and_texts() {
        let mut h = HeaderMap::new();
        assert_eq!(Lang::of(&h), Lang::En);
        h.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de-DE, es;q=0.8, fr-FR;q=0.9, en;q=0.5"));
        assert_eq!(Lang::of(&h), Lang::Fr);
        h.insert(header::COOKIE, HeaderValue::from_static("a=b; nuage_lang=es"));
        assert_eq!(Lang::of(&h), Lang::Es);
        assert_eq!(t(Lang::Fr, "Tokens"), "Jetons");
        assert_eq!(tf(Lang::Es, "Branch {} created.", &[&"x"]), "Rama x creada.");
        assert_eq!(tf(Lang::En, "{} of {}", &[&1, &2]), "1 of 2");
        // Every text has the same `{}` in the three languages, and no text is twice.
        let mut seen = std::collections::HashSet::new();
        for (en, fr, es) in TEXTS {
            assert!(seen.insert(*en), "twice: {en}");
            let n = en.matches("{}").count();
            assert_eq!((fr.matches("{}").count(), es.matches("{}").count()), (n, n), "{en}");
        }
    }
}
