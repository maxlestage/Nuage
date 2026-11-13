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
    ("Time", "Heure", "Hora"),
    ("Level", "Niveau", "Nivel"),
    ("Event", "Événement", "Evento"),
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
