//! The editor in English, French and Spanish.
//!
//! Texts are written in English in the code; [`t`] gives them in the chosen
//! language, [`tf`] fills their `{}` in order. A text without a translation
//! is a bug: debug builds (and the tests) stop on it, release builds show
//! the English.

use std::collections::HashMap;
use std::sync::OnceLock;

/// A language of the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Lang {
    #[default]
    En,
    Fr,
    Es,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Fr, Lang::Es];

    /// Its code, as `Accept-Language` says it.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Fr => "fr",
            Lang::Es => "es",
        }
    }

    /// The language of a locale (`fr-FR`, `es_ES.UTF-8`), English for any other.
    pub fn of_locale(locale: &str) -> Lang {
        let locale = locale.trim().to_ascii_lowercase();
        if locale.starts_with("fr") {
            Lang::Fr
        } else if locale.starts_with("es") {
            Lang::Es
        } else {
            Lang::En
        }
    }

    /// The first of French, Spanish or English in the order the person put
    /// their languages (macOS and Windows list several), else English.
    pub fn of_locales<S: AsRef<str>>(locales: impl IntoIterator<Item = S>) -> Lang {
        locales
            .into_iter()
            .find_map(|l| {
                let l = l.as_ref().trim().to_ascii_lowercase();
                [("fr", Lang::Fr), ("es", Lang::Es), ("en", Lang::En)]
                    .into_iter()
                    .find_map(|(code, lang)| l.starts_with(code).then_some(lang))
            })
            .unwrap_or(Lang::En)
    }

    /// The language of this computer.
    pub fn of_system() -> Lang {
        Lang::of_locales(sys_locale::get_locales())
    }
}

/// `en` in the language `l`.
pub fn t(l: Lang, en: &'static str) -> &'static str {
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

/// `en` in the language `l`, its `{}` filled with `args` in order.
pub fn tf(l: Lang, en: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut parts = t(l, en).split("{}").peekable();
    while let Some(part) = parts.next() {
        out.push_str(part);
        if parts.peek().is_some()
            && let Some(a) = args.next()
        {
            out.push_str(&a.to_string());
        }
    }
    out
}

fn table() -> &'static HashMap<&'static str, (&'static str, &'static str)> {
    static TABLE: OnceLock<HashMap<&'static str, (&'static str, &'static str)>> = OnceLock::new();
    TABLE.get_or_init(|| TEXTS.iter().map(|(en, fr, es)| (*en, (*fr, *es))).collect())
}

/// English, French, Spanish.
const TEXTS: &[(&str, &str, &str)] = &[
    ("This message is also in {}.", "Ce message est aussi dans {}.", "Este mensaje también está en {}."),
    ("Theme", "Thème", "Tema"),
    ("Language", "Langue", "Idioma"),
    (
        "Saved: in force now, with no restart.",
        "Enregistré : en vigueur tout de suite, sans redémarrage.",
        "Guardado: en vigor al momento, sin reiniciar.",
    ),
    (
        "Application {} created, with its own encrypted database.",
        "Application {} créée, avec sa propre base chiffrée.",
        "Aplicación {} creada, con su propia base cifrada.",
    ),
    ("New token for {}.", "Nouveau jeton pour {}.", "Nuevo token para {}."),
    (
        "Token {}… of {} revoked: it opens nothing any more.",
        "Jeton {}… de {} révoqué : il n'ouvre plus rien.",
        "Token {}… de {} revocado: ya no abre nada.",
    ),
    (
        "Memory of {} saved: in force now.",
        "Mémoire de {} enregistrée : en vigueur dès maintenant.",
        "Memoria de {} guardada: en vigor desde ahora.",
    ),
    ("Type its management token first.", "Tapez d'abord son jeton de gestion.", "Escribe primero su token de gestión."),
    (
        "Type the address of the server first.",
        "Tapez d'abord l'adresse du serveur.",
        "Escribe primero la dirección del servidor.",
    ),
    ("Connected to {}", "Connecté à {}", "Conectado a {}"),
    ("Not connected", "Non connecté", "Sin conexión"),
    ("Disconnect", "Se déconnecter", "Desconectar"),
    ("Open the console in my browser", "Ouvrir la console dans mon navigateur", "Abrir la consola en mi navegador"),
    ("Refresh", "Actualiser", "Actualizar"),
    ("On this computer", "Sur cet ordinateur", "En este equipo"),
    ("Management token", "Jeton de gestion", "Token de gestión"),
    ("Show", "Afficher", "Mostrar"),
    ("Remember on this computer", "Retenir sur cet ordinateur", "Recordar en este equipo"),
    ("Connect", "Se connecter", "Conectar"),
    ("Servers you used", "Serveurs déjà utilisés", "Servidores que has usado"),
    ("Forget", "Oublier", "Olvidar"),
    ("Theme: as the system", "Thème : comme le système", "Tema: como el sistema"),
    ("Theme: light", "Thème : clair", "Tema: claro"),
    ("Theme: dark", "Thème : sombre", "Tema: oscuro"),
    (
        "No server was installed on this computer by the installer.",
        "Aucun serveur n'a été installé sur cet ordinateur par l'installateur.",
        "El instalador no ha instalado ningún servidor en este equipo.",
    ),
    (
        "Its settings file is for administrators only.",
        "Son fichier de réglages est réservé aux administrateurs.",
        "Su archivo de ajustes es solo para administradores.",
    ),
    (
        "Its settings file could not be read: {}",
        "Son fichier de réglages n'a pas pu être lu : {}",
        "No se pudo leer su archivo de ajustes: {}",
    ),
    ("Not saved:", "Non enregistré :", "No guardado:"),
    ("Not found:", "Introuvable :", "No encontrado:"),
    (
        "Saved here: it wins over the environment ({}={}).",
        "Enregistré ici : il l'emporte sur l'environnement ({}={}).",
        "Guardado aquí: tiene prioridad sobre el entorno ({}={}).",
    ),
    (
        "Saved here ({} is not set in the environment).",
        "Enregistré ici ({} n'est pas réglé dans l'environnement).",
        "Guardado aquí ({} no está definido en el entorno).",
    ),
    ("From the environment ({}={}).", "Vient de l'environnement ({}={}).", "Viene del entorno ({}={})."),
    ("Default ({} is not set).", "Valeur par défaut ({} n'est pas réglé).", "Valor por defecto ({} no está definido)."),
    ("Back to the environment's value", "Revenir à la valeur de l'environnement", "Volver al valor del entorno"),
    ("Back to the default", "Revenir à la valeur par défaut", "Volver al valor por defecto"),
    ("Nothing changed.", "Rien n'a changé.", "Nada ha cambiado."),
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
    ("unknown", "inconnu", "desconocido"),
    (
        "Memory Nuage DB may count on",
        "Mémoire sur laquelle Nuage DB peut compter",
        "Memoria con la que Nuage DB puede contar",
    ),
    (
        "auto: what the system reports ({}), or NUAGE_MEMORY_MB when it is set. An amount leaves the rest to the other programs.",
        "auto : ce que le système annonce ({}), ou NUAGE_MEMORY_MB s'il est réglé. Une quantité laisse le reste aux autres programmes.",
        "auto: lo que indica el sistema ({}), o NUAGE_MEMORY_MB si está definido. Una cantidad deja el resto a los demás programas.",
    ),
    (
        "Page caches, all the applications together",
        "Caches de pages, toutes les applications ensemble",
        "Cachés de páginas, todas las aplicaciones juntas",
    ),
    (
        "auto follows the memory: a quarter of it at first, up to six tenths while there is room, less when it gets tight. An amount is a maximum. Now: {} (up to {}).",
        "auto suit la mémoire : un quart au départ, jusqu'à six dixièmes tant qu'il y a de la place, moins quand elle se resserre. Une quantité est un maximum. En ce moment : {} (jusqu'à {}).",
        "auto sigue a la memoria: un cuarto al principio, hasta seis décimos mientras haya sitio, menos cuando escasea. Una cantidad es un máximo. Ahora: {} (hasta {}).",
    ),
    (
        "Key-value store of each application",
        "Stockage clé-valeur de chaque application",
        "Almacén clave-valor de cada aplicación",
    ),
    (
        "auto: a sixteenth of the memory each, an eighth for all of them together. Lowering it drops the least recently used keys at once.",
        "auto : un seizième de la mémoire chacun, un huitième pour tous ensemble. Le baisser retire tout de suite les clés les moins récemment utilisées.",
        "auto: un dieciseisavo de la memoria cada uno, un octavo para todos juntos. Bajarlo quita al momento las claves usadas hace más tiempo.",
    ),
    ("Save the memory", "Enregistrer la mémoire", "Guardar la memoria"),
    ("Memory by application", "Mémoire par application", "Memoria por aplicación"),
    (
        "Each application shares the page caches of the server by use (auto), or has an amount set apart for it alone.",
        "Chaque application partage les caches de pages du serveur selon l'usage (auto), ou dispose d'une part réservée pour elle seule.",
        "Cada aplicación comparte las cachés de páginas del servidor según el uso (auto), o tiene una parte reservada solo para ella.",
    ),
    ("Save", "Enregistrer", "Guardar"),
    ("Memory of one statement", "Mémoire d'une instruction", "Memoria de una sentencia"),
    (
        "What one SQL statement may hold at once; past it, the statement fails with a clear error instead of filling the memory. auto: an eighth of the memory.",
        "Ce qu'une instruction SQL peut garder en mémoire à la fois ; au-delà, elle échoue avec une erreur claire au lieu de remplir la mémoire. auto : un huitième de la mémoire.",
        "Lo que una sentencia SQL puede retener a la vez; más allá, falla con un error claro en lugar de llenar la memoria. auto: un octavo de la memoria.",
    ),
    ("Seconds a statement may run", "Secondes qu'une instruction peut durer", "Segundos que puede durar una sentencia"),
    (
        "Past it, the statement stops with an error and its changes are undone. 0: no limit.",
        "Au-delà, l'instruction s'arrête avec une erreur et ses changements sont annulés. 0 : pas de limite.",
        "Más allá, la sentencia se detiene con un error y sus cambios se deshacen. 0: sin límite.",
    ),
    ("PostgreSQL connections at once", "Connexions PostgreSQL simultanées", "Conexiones PostgreSQL simultáneas"),
    ("Save the limits", "Enregistrer les limites", "Guardar los límites"),
    ("Visibility", "Visibilité", "Visibilidad"),
    ("New application", "Nouvelle application", "Nueva aplicación"),
    ("Name", "Nom", "Nombre"),
    ("Create the application", "Créer l'application", "Crear la aplicación"),
    ("Tokens", "Jetons", "Tokens"),
    (
        "No token: no program can connect yet.",
        "Aucun jeton : aucun programme ne peut encore se connecter.",
        "Ningún token: ningún programa puede conectarse aún.",
    ),
    ("read only", "lecture seule", "solo lectura"),
    ("read and write", "lecture et écriture", "lectura y escritura"),
    ("Revoke", "Révoquer", "Revocar"),
    ("New token (read and write)", "Nouveau jeton (lecture et écriture)", "Nuevo token (lectura y escritura)"),
    (
        "Read and write: for a server or a program, never in a web page.",
        "Lecture et écriture : pour un serveur ou un programme, jamais dans une page web.",
        "Lectura y escritura: para un servidor o un programa, nunca en una página web.",
    ),
    ("Copy the token", "Copier le jeton", "Copiar el token"),
    ("Revoke this token?", "Révoquer ce jeton ?", "¿Revocar este token?"),
    ("Cancel", "Annuler", "Cancelar"),
    (
        "The server at {} answers, and its management token is right.",
        "Le serveur à l'adresse {} répond, et son jeton de gestion est le bon.",
        "El servidor en {} responde, y su token de gestión es correcto.",
    ),
    ("Memory", "Mémoire", "Memoria"),
    ("{} used, {} free of {}", "{} utilisés, {} libres sur {}", "{} usados, {} libres de {}"),
    ("Applications", "Applications", "Aplicaciones"),
    ("Backups", "Sauvegardes", "Copias de seguridad"),
    (
        "The settings of this server are those of its environment (NUAGE_SETTINGS=environment): the editor shows them, and changes nothing.",
        "Les réglages de ce serveur sont ceux de son environnement (NUAGE_SETTINGS=environment) : l'éditeur les montre, et ne change rien.",
        "Los ajustes de este servidor son los de su entorno (NUAGE_SETTINGS=environment): el editor los muestra, y no cambia nada.",
    ),
    (
        "{} setting(s) saved earlier are ignored while the environment rules.",
        "{} réglage(s) enregistré(s) plus tôt sont ignorés tant que l'environnement fait foi.",
        "{} ajuste(s) guardado(s) antes se ignoran mientras manda el entorno.",
    ),
    ("Variable", "Variable", "Variable"),
    ("Value", "Valeur", "Valor"),
    ("set (a secret: not shown)", "réglé (un secret : non affiché)", "definido (un secreto: no se muestra)"),
    ("Overview", "Vue d'ensemble", "Resumen"),
    ("Limits and visibility", "Limites et visibilité", "Límites y visibilidad"),
    ("Started with", "Réglés au démarrage", "Definidos al arrancar"),
    ("Application", "Application", "Aplicación"),
    ("Its cache holds", "Son cache contient", "Su caché contiene"),
    ("may hold", "peut contenir", "puede contener"),
    ("Page cache", "Cache de pages", "Caché de páginas"),
    ("Key-value store", "Stockage clé-valeur", "Almacén clave-valor"),
    ("({} dropped)", "({} retiré)", "(sin {})"),
    (
        "A file of an early version of the editor, with tokens in clear, could not be deleted ({}): delete it yourself.",
        "Un fichier d'une première version de l'éditeur, avec des jetons en clair, n'a pas pu être supprimé ({}) : supprimez-le vous-même.",
        "No se pudo borrar un archivo de una primera versión del editor, con tokens en claro ({}): bórralo tú.",
    ),
    (
        "A management token has letters, digits and signs only, without spaces.",
        "Un jeton de gestion n'a que des lettres, des chiffres et des signes, sans espace.",
        "Un token de gestión solo tiene letras, cifras y signos, sin espacios.",
    ),
    (
        "A management token has {} characters at least: is part of it missing?",
        "Un jeton de gestion a au moins {} caractères : en manque-t-il une partie ?",
        "Un token de gestión tiene al menos {} caracteres: ¿falta una parte?",
    ),
    (
        "A server that was asleep (Heroku Eco) can take half a minute to wake.",
        "Un serveur endormi (Heroku Eco) peut mettre une demi-minute à se réveiller.",
        "Un servidor dormido (Heroku Eco) puede tardar medio minuto en despertar.",
    ),
    (
        "An early version of the editor kept tokens in a file, in clear: that file is now deleted. Type your token again; tick \"Remember on this computer\" to keep it in your system's password store.",
        "Une première version de l'éditeur gardait des jetons dans un fichier, en clair : ce fichier est maintenant supprimé. Tapez à nouveau votre jeton ; cochez « Retenir sur cet ordinateur » pour le garder dans le gestionnaire de mots de passe de votre système.",
        "Una primera versión del editor guardaba tokens en un archivo, en claro: ese archivo se ha borrado. Escribe tu token de nuevo; marca «Recordar en este equipo» para guardarlo en el almacén de contraseñas de tu sistema.",
    ),
    ("Connect anyway", "Se connecter quand même", "Conectar de todos modos"),
    (
        "Connected, but the token could not be kept in {} ({}): it is used until you close the editor.",
        "Connecté, mais le jeton n'a pas pu être gardé dans {} ({}) : il sert jusqu'à la fermeture de l'éditeur.",
        "Conectado, pero no se pudo guardar el token en {} ({}): se usa hasta que cierres el editor.",
    ),
    (
        "Copied, but the clipboard history could not be asked to skip it ({}): it may keep it.",
        "Copié, mais l'historique du presse-papiers n'a pas pu être prié de l'ignorer ({}) : il peut le garder.",
        "Copiado, pero no se pudo pedir al historial del portapapeles que lo ignore ({}): puede guardarlo.",
    ),
    (
        "Copied. It stays in the clipboard until you copy something else; the clipboard history is asked to skip it.",
        "Copié. Il reste dans le presse-papiers jusqu'à la prochaine copie ; l'historique du presse-papiers est prié de l'ignorer.",
        "Copiado. Se queda en el portapapeles hasta que copies otra cosa; se pide al historial del portapapeles que lo ignore.",
    ),
    ("Forget this token", "Oublier ce jeton", "Olvidar este token"),
    (
        "Looking for a Nuage DB server installed here…",
        "Recherche d'un serveur Nuage DB installé ici…",
        "Buscando un servidor Nuage DB instalado aquí…",
    ),
    (
        "Looking for this system's password store…",
        "Recherche du gestionnaire de mots de passe du système…",
        "Buscando el almacén de contraseñas del sistema…",
    ),
    (
        "No computer has the name {}: check the spelling, and that this computer is on the Internet.",
        "Aucun ordinateur ne porte le nom {} : vérifiez l'orthographe, et que cet ordinateur est sur Internet.",
        "Ningún equipo se llama {}: revisa la ortografía, y que este equipo esté conectado a Internet.",
    ),
    (
        "No token is kept for this server any more: type it again.",
        "Aucun jeton n'est plus gardé pour ce serveur : tapez-le à nouveau.",
        "Ya no hay ningún token guardado para este servidor: escríbelo de nuevo.",
    ),
    (
        "Not encrypted: the token would cross the Internet in clear. Use the server's https:// address.",
        "Non chiffré : le jeton traverserait Internet en clair. Utilisez l'adresse https:// du serveur.",
        "Sin cifrar: el token cruzaría Internet en claro. Usa la dirección https:// del servidor.",
    ),
    (
        "Nothing answers at {}: is the server running, and is the port right?",
        "Rien ne répond à {} : le serveur est-il démarré, et le port est-il le bon ?",
        "Nada responde en {}: ¿está en marcha el servidor y es correcto el puerto?",
    ),
    (
        "Nuage DB does not answer on this computer (port {}): it is stopped, or still starting.",
        "Nuage DB ne répond pas sur cet ordinateur (port {}) : il est arrêté, ou encore en train de démarrer.",
        "Nuage DB no responde en este equipo (puerto {}): está detenido, o todavía arrancando.",
    ),
    ("Stop waiting", "Ne plus attendre", "Dejar de esperar"),
    (
        "The answer is larger than any of Nuage DB's (1 MiB): is this address really your Nuage DB server?",
        "La réponse est plus grande que toutes celles de Nuage DB (1 Mio) : cette adresse est-elle bien celle de votre serveur Nuage DB ?",
        "La respuesta es más grande que cualquiera de Nuage DB (1 MiB): ¿es de verdad esta la dirección de tu servidor Nuage DB?",
    ),
    (
        "The connection failed: check this computer's network, then try again.",
        "La connexion a échoué : vérifiez le réseau de cet ordinateur, puis réessayez.",
        "La conexión ha fallado: revisa la red de este equipo y vuelve a intentarlo.",
    ),
    (
        "The editor failed unexpectedly (a bug). Try again; if it happens again, please report it.",
        "L'éditeur a échoué de façon inattendue (un bug). Réessayez ; si cela se reproduit, merci de le signaler.",
        "El editor ha fallado de forma inesperada (un error del programa). Vuelve a intentarlo; si se repite, avísanos.",
    ),
    (
        "The editor speaks http:// and https:// only, not {}://.",
        "L'éditeur ne parle que http:// et https://, pas {}://.",
        "El editor solo habla http:// y https://, no {}://.",
    ),
    (
        "The host answered {} with a page of its own, not Nuage DB's answer: something between this computer and the server (a proxy) refuses the call.",
        "L'hébergeur a répondu {} avec une page à lui, pas la réponse de Nuage DB : quelque chose entre cet ordinateur et le serveur (un proxy) refuse l'appel.",
        "El alojamiento respondió {} con una página propia, no la respuesta de Nuage DB: algo entre este equipo y el servidor (un proxy) rechaza la llamada.",
    ),
    (
        "The host answered {}: the server is starting, asleep or crashed. Try again in a minute (Heroku: More, then View logs).",
        "L'hébergeur a répondu {} : le serveur démarre, dort ou s'est arrêté. Réessayez dans une minute (Heroku : More, puis View logs).",
        "El alojamiento respondió {}: el servidor está arrancando, dormido o se ha caído. Vuelve a intentarlo en un minuto (Heroku: More, luego View logs).",
    ),
    (
        "The kept token no longer works: it was changed on the server. Type the new one; it replaces the kept one once it works.",
        "Le jeton gardé ne fonctionne plus : il a été changé sur le serveur. Tapez le nouveau ; il remplacera le jeton gardé dès qu'il fonctionnera.",
        "El token guardado ya no funciona: se cambió en el servidor. Escribe el nuevo; sustituirá al guardado en cuanto funcione.",
    ),
    (
        "The password store did not forget the token: {}",
        "Le gestionnaire de mots de passe n'a pas oublié le jeton : {}",
        "El almacén de contraseñas no ha olvidado el token: {}",
    ),
    (
        "The password store did not give the kept token: type it instead.",
        "Le gestionnaire de mots de passe n'a pas donné le jeton gardé : tapez-le plutôt.",
        "El almacén de contraseñas no ha dado el token guardado: escríbelo en su lugar.",
    ),
    (
        "The secure connection (https) failed: the server's certificate is not trusted, or nothing speaks https at this address (a server of your own network may need http://). A network that inspects HTTPS causes this too. The editor never ignores a certificate.",
        "La connexion sécurisée (https) a échoué : le certificat du serveur n'est pas reconnu, ou rien ne parle https à cette adresse (un serveur de votre propre réseau peut demander http://). Un réseau qui inspecte le HTTPS le provoque aussi. L'éditeur n'ignore jamais un certificat.",
        "La conexión segura (https) ha fallado: el certificado del servidor no es de confianza, o nada habla https en esta dirección (un servidor de tu propia red puede necesitar http://). Una red que inspecciona HTTPS también lo provoca. El editor nunca ignora un certificado.",
    ),
    (
        "The server did not answer in time: the change may or may not have been made. The page is read again to show what is in force.",
        "Le serveur n'a pas répondu à temps : le changement a peut-être été fait, ou peut-être pas. La page est relue pour montrer ce qui est en vigueur.",
        "El servidor no respondió a tiempo: puede que el cambio se haya hecho o no. La página se vuelve a leer para mostrar lo que está en vigor.",
    ),
    ("The server failed (HTTP {}).", "Le serveur a échoué (HTTP {}).", "El servidor ha fallado (HTTP {})."),
    (
        "The server refused this token. The editor needs the management token (NUAGE_ADMIN_TOKEN), not an application's token nor the console password.",
        "Le serveur a refusé ce jeton. L'éditeur a besoin du jeton de gestion (NUAGE_ADMIN_TOKEN), pas du jeton d'une application ni du mot de passe de la console.",
        "El servidor ha rechazado este token. El editor necesita el token de gestión (NUAGE_ADMIN_TOKEN), no el token de una aplicación ni la contraseña de la consola.",
    ),
    ("The token goes in {}.", "Le jeton va dans {}.", "El token se guarda en {}."),
    (
        "This is an application's token (NUAGE_TOKEN), not the management token.",
        "Ceci est le jeton d'une application (NUAGE_TOKEN), pas le jeton de gestion.",
        "Este es el token de una aplicación (NUAGE_TOKEN), no el token de gestión.",
    ),
    (
        "This server (before the settings came, 0.4.7) lets the editor manage applications and tokens only: update it for memory and limits.",
        "Ce serveur (antérieur aux réglages, 0.4.7) ne laisse l'éditeur gérer que les applications et les jetons : mettez-le à jour pour la mémoire et les limites.",
        "Este servidor (anterior a los ajustes, 0.4.7) solo deja al editor gestionar las aplicaciones y los tokens: actualízalo para la memoria y los límites.",
    ),
    (
        "This server is older than Nuage DB 0.4.2: update it to manage it from here.",
        "Ce serveur est antérieur à Nuage DB 0.4.2 : mettez-le à jour pour le gérer d'ici.",
        "Este servidor es anterior a Nuage DB 0.4.2: actualízalo para gestionarlo desde aquí.",
    ),
    (
        "This server takes its settings from its environment only (NUAGE_SETTINGS=environment): nothing is changed from here.",
        "Ce serveur prend ses réglages de son environnement seulement (NUAGE_SETTINGS=environment) : rien ne se change d'ici.",
        "Este servidor toma sus ajustes solo de su entorno (NUAGE_SETTINGS=environment): nada se cambia desde aquí.",
    ),
    (
        "This server's management API is off: NUAGE_ADMIN_TOKEN is not set on it. Set it in its settings file or its host's variables (`nuage genkey` makes one), then restart the server.",
        "L'API de gestion de ce serveur est désactivée : NUAGE_ADMIN_TOKEN n'y est pas réglé. Réglez-le dans son fichier de réglages ou dans les variables de son hébergeur (`nuage genkey` en crée un), puis redémarrez le serveur.",
        "La API de gestión de este servidor está desactivada: NUAGE_ADMIN_TOKEN no está definido en él. Defínelo en su archivo de ajustes o en las variables de su alojamiento (`nuage genkey` crea uno) y reinicia el servidor.",
    ),
    (
        "This server's version cannot do this yet: update the server to do it here.",
        "La version de ce serveur ne sait pas encore le faire : mettez le serveur à jour pour le faire d'ici.",
        "La versión de este servidor aún no sabe hacerlo: actualiza el servidor para hacerlo desde aquí.",
    ),
    (
        "This token can make tokens for every application: remember it only on your own computer.",
        "Ce jeton peut créer des jetons pour toutes les applications : ne le retenez que sur votre propre ordinateur.",
        "Este token puede crear tokens para todas las aplicaciones: recuérdalo solo en tu propio equipo.",
    ),
    (
        "Token forgotten: it is no longer kept on this computer.",
        "Jeton oublié : il n'est plus gardé sur cet ordinateur.",
        "Token olvidado: ya no se guarda en este equipo.",
    ),
    ("Token kept in {}.", "Jeton gardé dans {}.", "Token guardado en {}."),
    (
        "Too many calls are still waiting for an answer: try again in a moment.",
        "Trop d'appels attendent encore une réponse : réessayez dans un instant.",
        "Demasiadas llamadas esperan todavía una respuesta: vuelve a intentarlo en un momento.",
    ),
    ("Type another token", "Taper un autre jeton", "Escribir otro token"),
    (
        "Update the server to see this here: its version has no settings for the editor (they came with Nuage DB 0.4.7).",
        "Mettez le serveur à jour pour voir ceci ici : sa version n'a pas de réglages pour l'éditeur (ils sont arrivés avec Nuage DB 0.4.7).",
        "Actualiza el servidor para ver esto aquí: su versión no tiene ajustes para el editor (llegaron con Nuage DB 0.4.7).",
    ),
    ("Use {}", "Utiliser {}", "Usar {}"),
    ("Waiting for your keyring…", "En attente de votre trousseau…", "Esperando a tu llavero…"),
    ("Will connect to {}", "Connexion à {}", "Se conectará a {}"),
    (
        "Windows Credential Manager",
        "le Gestionnaire d'identification de Windows",
        "el Administrador de credenciales de Windows",
    ),
    (
        "Your system has no password store the editor can use ({}): the token is kept until you close the editor.",
        "Votre système n'a pas de gestionnaire de mots de passe que l'éditeur puisse utiliser ({}) : le jeton est gardé jusqu'à la fermeture de l'éditeur.",
        "Tu sistema no tiene un almacén de contraseñas que el editor pueda usar ({}): el token se guarda hasta que cierres el editor.",
    ),
    (
        "\"{}\" is not an address. Example: https://my-nuage.herokuapp.com",
        "« {} » n'est pas une adresse. Exemple : https://mon-nuage.herokuapp.com",
        "«{}» no es una dirección. Ejemplo: https://mi-nuage.herokuapp.com",
    ),
    (
        "the editor's memory, until it closes",
        "la mémoire de l'éditeur, jusqu'à sa fermeture",
        "la memoria del editor, hasta que se cierre",
    ),
    ("token kept", "jeton gardé", "token guardado"),
    (
        "your desktop's keyring (Secret Service)",
        "le trousseau de votre bureau (Secret Service)",
        "el llavero de tu escritorio (Secret Service)",
    ),
    ("your login Keychain", "votre trousseau de session", "tu llavero de inicio de sesión"),
    (
        "{} answers, but not as a Nuage DB server. Is it your website's address rather than Nuage DB's?",
        "{} répond, mais pas comme un serveur Nuage DB. Est-ce l'adresse de votre site plutôt que celle de Nuage DB ?",
        "{} responde, pero no como un servidor Nuage DB. ¿Es la dirección de tu sitio web en lugar de la de Nuage DB?",
    ),
    (
        "{} did not answer within {}; a server that was asleep can take half a minute to wake. Try again.",
        "{} n'a pas répondu en {} ; un serveur endormi peut mettre une demi-minute à se réveiller. Réessayez.",
        "{} no respondió en {}; un servidor dormido puede tardar medio minuto en despertar. Vuelve a intentarlo.",
    ),
    (
        "{} is not a port: a port is a number from 1 to 65535.",
        "{} n'est pas un port : un port est un nombre de 1 à 65535.",
        "{} no es un puerto: un puerto es un número del 1 al 65535.",
    ),
    (
        "{} sends the editor to {}, its https:// address: use that one (the editor does not follow it with your token by itself).",
        "{} renvoie l'éditeur vers {}, son adresse https:// : utilisez-la (l'éditeur ne la suit pas tout seul avec votre jeton).",
        "{} envía el editor a {}, su dirección https://: usa esa (el editor no la sigue por su cuenta con tu token).",
    ),
    (
        "{} sends the editor to {}. The editor does not follow it with your token: if that is your server, type its address.",
        "{} renvoie l'éditeur vers {}. L'éditeur ne la suit pas avec votre jeton : si c'est votre serveur, tapez son adresse.",
        "{} envía el editor a {}. El editor no la sigue con tu token: si es tu servidor, escribe su dirección.",
    ),
    (
        "Nuage DB Editor could not open its window on this computer (graphics: {}). The web console works in a browser: http://127.0.0.1:8080/admin for a server on this computer.",
        "Nuage DB Éditeur n'a pas pu ouvrir sa fenêtre sur cet ordinateur (graphismes : {}). La console web fonctionne dans un navigateur : http://127.0.0.1:8080/admin pour un serveur de cet ordinateur.",
        "Nuage DB Editor no pudo abrir su ventana en este equipo (gráficos: {}). La consola web funciona en un navegador: http://127.0.0.1:8080/admin para un servidor de este equipo.",
    ),
    (
        "Nuage DB Editor reads the port and the management token of the Nuage DB server of this computer (not its encryption key).",
        "Nuage DB Éditeur lit le port et le jeton de gestion du serveur Nuage DB de cet ordinateur (pas sa clé de chiffrement).",
        "Nuage DB Editor lee el puerto y el token de gestión del servidor Nuage DB de este equipo (no su clave de cifrado).",
    ),
    (
        "Nuage DB Editor starts the Nuage DB server of this computer.",
        "Nuage DB Éditeur démarre le serveur Nuage DB de cet ordinateur.",
        "Nuage DB Editor inicia el servidor Nuage DB de este equipo.",
    ),
    ("What failed: {}", "Ce qui a échoué : {}", "Lo que falló: {}"),
    ("Where: {}", "Où : {}", "Dónde: {}"),
    ("Back to the start", "Revenir au début", "Volver al inicio"),
    ("Copy the details", "Copier les détails", "Copiar los detalles"),
    ("Quit", "Quitter", "Salir"),
    (
        "Tell the editor's authors (GitHub issues)",
        "Le signaler aux auteurs de l'éditeur (GitHub issues)",
        "Avisar a los autores del editor (GitHub issues)",
    ),
    (
        "Nothing answers on port {} of this computer right now: the server may be stopped.",
        "Rien ne répond sur le port {} de cet ordinateur pour l'instant : le serveur est peut-être arrêté.",
        "Nada responde en el puerto {} de este equipo ahora mismo: puede que el servidor esté detenido.",
    ),
    ("{} s", "{} s", "{} s"),
    ("{} min", "{} min", "{} min"),
    ("{} h", "{} h", "{} h"),
    ("{} day", "{} jour", "{} día"),
    ("{} days", "{} jours", "{} días"),
    ("{} ago", "il y a {}", "hace {}"),
    (
        "An application is a database for one website or program: its own data, encrypted, and its own tokens. Applications never see each other's data.",
        "Une application, c'est une base de données pour un site ou un programme : ses propres données, chiffrées, et ses propres jetons. Les applications ne voient jamais les données des autres.",
        "Una aplicación es una base de datos para un sitio web o un programa: sus propios datos, cifrados, y sus propios tokens. Las aplicaciones nunca ven los datos de las demás.",
    ),
    ("Open its page", "Ouvrir sa page", "Abrir su página"),
    ("main", "principale", "principal"),
    (
        "created at installation; it cannot be deleted",
        "créée à l'installation ; elle ne peut pas être supprimée",
        "creada en la instalación; no se puede eliminar",
    ),
    ("tables {} · rows {} · {}", "tables {} · lignes {} · {}", "tablas {} · filas {} · {}"),
    ("Backup failing", "Sauvegarde en échec", "Copia de seguridad fallida"),
    (
        "For now there is only the main application, {}, created at installation. Make one per website, with New application above.",
        "Pour l'instant il n'y a que l'application principale, {}, créée à l'installation. Créez-en une par site, avec Nouvelle application ci-dessus.",
        "Por ahora solo existe la aplicación principal, {}, creada en la instalación. Crea una por sitio web, con Nueva aplicación arriba.",
    ),
    (
        "An application missing here? The web console lists those the server could not open.",
        "Une application manque ? La console web liste celles que le serveur n'a pas pu ouvrir.",
        "¿Falta una aplicación? La consola web lista las que el servidor no pudo abrir.",
    ),
    ("What is it for?", "À quoi sert-elle ?", "¿Para qué sirve?"),
    ("My shop", "Ma boutique", "Mi tienda"),
    ("Its name will be: {}", "Son nom sera : {}", "Su nombre será: {}"),
    (
        "1 to 63 lowercase letters, digits, - and _ (not main). It is also its database name for PostgreSQL.",
        "1 à 63 lettres minuscules, chiffres, - et _ (pas main). C'est aussi son nom de base pour PostgreSQL.",
        "De 1 a 63 letras minúsculas, cifras, - y _ (no main). También es su nombre de base de datos para PostgreSQL.",
    ),
    ("Creating…", "Création…", "Creando…"),
    (
        "It was created, but its token never reached the editor: make it a new token, then revoke the lost one (made at {}).",
        "Elle a été créée, mais son jeton n'est jamais arrivé jusqu'à l'éditeur : créez-lui un nouveau jeton, puis révoquez celui qui est perdu (créé à {}).",
        "Se creó, pero su token nunca llegó al editor: crea un token nuevo y revoca después el perdido (creado a las {}).",
    ),
    ("Open {}", "Ouvrir {}", "Abrir {}"),
    (
        "The server did not answer in time. The application may have been created anyway.",
        "Le serveur n'a pas répondu à temps. L'application a peut-être été créée quand même.",
        "El servidor no respondió a tiempo. Puede que la aplicación se haya creado de todos modos.",
    ),
    ("Check", "Vérifier", "Comprobar"),
    ("{} token", "{} jeton", "{} token"),
    ("{} tokens", "{} jetons", "{} tokens"),
    ("{}, {} read-only", "{}, dont {} en lecture seule", "{}, {} de solo lectura"),
    ("Waiting for a first connection", "En attente d'une première connexion", "Esperando una primera conexión"),
    (
        "Tested from the editor {} — your site has not connected yet",
        "Testée depuis l'éditeur {} — votre site ne s'est pas encore connecté",
        "Probada desde el editor {}: tu sitio aún no se ha conectado",
    ),
    ("Connected {} ({})", "Connectée {} ({})", "Conectada {} ({})"),
    (
        "There is no application {} any more (deleted from the console?).",
        "Il n'y a plus d'application {} (supprimée depuis la console ?).",
        "Ya no hay ninguna aplicación {} (¿eliminada desde la consola?).",
    ),
    ("Connection", "Connexion", "Conexión"),
    ("a token of this application", "un jeton de cette application", "un token de esta aplicación"),
    (
        "A token is shown only when it is made: make a new one to connect another site.",
        "Un jeton n'est affiché qu'à sa création : créez-en un nouveau pour brancher un autre site.",
        "Un token solo se muestra al crearlo: crea uno nuevo para conectar otro sitio.",
    ),
    (
        "The token also works as the PostgreSQL password of this application (its database name is its name).",
        "Le jeton sert aussi de mot de passe PostgreSQL pour cette application (son nom de base est son nom).",
        "El token también sirve como contraseña de PostgreSQL de esta aplicación (su nombre de base de datos es su nombre).",
    ),
    ("Delete", "Supprimer", "Eliminar"),
    (
        "The main application cannot be deleted.",
        "L'application principale ne peut pas être supprimée.",
        "La aplicación principal no se puede eliminar.",
    ),
    (
        "Deleting an application erases its database and its backups. It is done in the web console, where you type its name again to confirm. The console asks for its password (NUAGE_ADMIN_PASSWORD, set with the server).",
        "Supprimer une application efface sa base et ses sauvegardes. Cela se fait dans la console web, où l'on retape son nom pour confirmer. La console demande son mot de passe (NUAGE_ADMIN_PASSWORD, réglé avec le serveur).",
        "Eliminar una aplicación borra su base de datos y sus copias de seguridad. Se hace en la consola web, donde vuelves a escribir su nombre para confirmar. La consola pide su contraseña (NUAGE_ADMIN_PASSWORD, definida con el servidor).",
    ),
    (
        "Open the console on this application",
        "Ouvrir la console sur cette application",
        "Abrir la consola en esta aplicación",
    ),
    (
        "NUAGE_TOKEN (set on the server): read and write; it changes in the server's settings, not here.",
        "NUAGE_TOKEN (réglé sur le serveur) : lecture et écriture ; il se change dans les réglages du serveur, pas ici.",
        "NUAGE_TOKEN (definido en el servidor): lectura y escritura; se cambia en los ajustes del servidor, no aquí.",
    ),
    ("{} · made {}", "{} · créé le {}", "{} · creado el {}"),
    (
        "New read-only token, for a web page",
        "Nouveau jeton en lecture seule, pour une page web",
        "Nuevo token de solo lectura, para una página web",
    ),
    (
        "A read-and-write token stays on a server. A web page shows its code to every visitor: give it a read-only token, which can only read.",
        "Un jeton lecture-écriture reste sur un serveur. Une page web montre son code à tous ses visiteurs : donnez-lui un jeton en lecture seule, qui ne peut que lire.",
        "Un token de lectura y escritura se queda en un servidor. Una página web muestra su código a todos sus visitantes: dale un token de solo lectura, que solo puede leer.",
    ),
    ("Websites allowed", "Sites web autorisés", "Sitios web autorizados"),
    (
        "Pages of these websites may call this database from their visitors' browsers. Write the start of the address, without a path: https://monsite.fr; to try on this computer, http://localhost:3000.",
        "Les pages de ces sites peuvent appeler cette base depuis le navigateur de leurs visiteurs. Écrivez le début de l'adresse, sans chemin : https://monsite.fr ; pour essayer sur cet ordinateur, http://localhost:3000.",
        "Las páginas de estos sitios pueden llamar a esta base de datos desde el navegador de sus visitantes. Escribe el principio de la dirección, sin ruta: https://misitio.es; para probar en este equipo, http://localhost:3000.",
    ),
    (
        "None yet: browsers refuse the calls of every page.",
        "Aucun pour l'instant : les navigateurs refusent les appels de toutes les pages.",
        "Ninguno por ahora: los navegadores rechazan las llamadas de todas las páginas.",
    ),
    ("any website", "n'importe quel site", "cualquier sitio"),
    ("Remove", "Retirer", "Quitar"),
    (
        "Your site is in https and Nuage DB in http: browsers will block the calls. Nuage DB needs an https address.",
        "Votre site est en https et Nuage DB en http : les navigateurs bloqueront les appels. Il faut une adresse https pour Nuage DB.",
        "Tu sitio está en https y Nuage DB en http: los navegadores bloquearán las llamadas. Nuage DB necesita una dirección https.",
    ),
    ("Website to allow", "Site à autoriser", "Sitio que autorizar"),
    ("Add", "Ajouter", "Añadir"),
    (
        "That is not the start of a website's address: write its scheme and its name, without a path, such as https://monsite.fr.",
        "Ce n'est pas le début de l'adresse d'un site : écrivez son schéma et son nom, sans chemin, comme https://monsite.fr.",
        "Eso no es el principio de la dirección de un sitio: escribe su esquema y su nombre, sin ruta, como https://misitio.es.",
    ),
    ("{} is already allowed.", "{} est déjà autorisé.", "{} ya está autorizado."),
    ("Open it on this application", "L'ouvrir sur cette application", "Abrirla en esta aplicación"),
    ("Memory of this application", "Mémoire de cette application", "Memoria de esta aplicación"),
    (
        "auto: shared with the others, by use. An amount: set apart for it (512 MiB, 2 GiB; a number alone is in MiB).",
        "auto : partagée avec les autres, selon l'usage. Une quantité : une part réservée (512 Mio, 2 Gio ; un nombre seul est en Mio).",
        "auto: compartida con las demás, según el uso. Una cantidad: una parte reservada (512 MiB, 2 GiB; un número solo va en MiB).",
    ),
    (
        "Its cache holds {} of {} now.",
        "Son cache contient {} sur {} en ce moment.",
        "Su caché contiene {} de {} ahora.",
    ),
    ("Save its memory", "Enregistrer sa mémoire", "Guardar su memoria"),
    ("Copy this token now", "Copiez ce jeton maintenant", "Copia este token ahora"),
    ("Application {} created", "Application {} créée", "Aplicación {} creada"),
    ("New token for {}", "Nouveau jeton pour {}", "Nuevo token para {}"),
    ("New read-only token for {}", "Nouveau jeton en lecture seule pour {}", "Nuevo token de solo lectura para {}"),
    (
        "Read only: it may sit in a web page; nobody can change or erase your data with it.",
        "Lecture seule : il peut figurer dans une page web ; personne ne pourra modifier ni effacer vos données avec lui.",
        "Solo lectura: puede estar en una página web; nadie podrá cambiar ni borrar tus datos con él.",
    ),
    (
        "It will never be shown again, here or in the console: the server keeps only a fingerprint of it. If you lose it, make another one.",
        "Il ne sera plus jamais affiché, ni ici ni dans la console : le serveur n'en garde qu'une empreinte. Si vous le perdez, créez-en un autre.",
        "No se volverá a mostrar nunca, ni aquí ni en la consola: el servidor solo guarda una huella. Si lo pierdes, crea otro.",
    ),
    (
        "For your website or program (its .env file, or its host's variables):",
        "Pour votre site ou programme (son fichier .env, ou les variables de son hébergeur) :",
        "Para tu sitio web o programa (su archivo .env, o las variables de su alojamiento):",
    ),
    ("Hide the token", "Masquer le jeton", "Ocultar el token"),
    ("Show the token", "Afficher le jeton", "Mostrar el token"),
    ("Copy both lines", "Copier les deux lignes", "Copiar las dos líneas"),
    ("Copy the token only", "Copier le jeton seul", "Copiar solo el token"),
    ("Test the token", "Tester le jeton", "Probar el token"),
    (
        "This address works for a program on this computer. On another device of your network, use http://{}:{} instead (if this computer's firewall lets it in). A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
        "Cette adresse fonctionne pour un programme de cet ordinateur. Sur un autre appareil de votre réseau, utilisez plutôt http://{}:{} (si le pare-feu de cet ordinateur le laisse entrer). Un site hébergé sur Internet ne peut pas joindre un Nuage DB de cet ordinateur : il lui faut une adresse https publique, ou un Nuage DB en ligne, par exemple sur Heroku.",
        "Esta dirección sirve para un programa de este equipo. En otro dispositivo de tu red, usa en su lugar http://{}:{} (si el cortafuegos de este equipo lo deja entrar). Un sitio alojado en Internet no puede llegar a un Nuage DB de este equipo: necesita una dirección https pública, o un Nuage DB en línea, por ejemplo en Heroku.",
    ),
    (
        "This address works for a program on this computer. A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
        "Cette adresse fonctionne pour un programme de cet ordinateur. Un site hébergé sur Internet ne peut pas joindre un Nuage DB de cet ordinateur : il lui faut une adresse https publique, ou un Nuage DB en ligne, par exemple sur Heroku.",
        "Esta dirección sirve para un programa de este equipo. Un sitio alojado en Internet no puede llegar a un Nuage DB de este equipo: necesita una dirección https pública, o un Nuage DB en línea, por ejemplo en Heroku.",
    ),
    ("In a web page", "Dans une page web", "En una página web"),
    (
        "No website may call this application yet: browsers will refuse calls from a page until you add its address under Websites allowed.",
        "Aucun site ne peut encore appeler cette application : les navigateurs refuseront les appels d'une page tant que vous n'aurez pas ajouté son adresse dans Sites web autorisés.",
        "Ningún sitio puede llamar todavía a esta aplicación: los navegadores rechazarán las llamadas de una página hasta que añadas su dirección en Sitios web autorizados.",
    ),
    ("Go to Websites allowed", "Aller aux sites web autorisés", "Ir a Sitios web autorizados"),
    ("Websites allowed: {}.", "Sites autorisés : {}.", "Sitios permitidos: {}."),
    (
        "Paste this in the page's script:",
        "Collez ceci dans le script de la page :",
        "Pega esto en el script de la página:",
    ),
    ("Copy for the web page", "Copier pour la page web", "Copiar para la página web"),
    (
        "Then restart your website or program so it reads these lines. Its application page shows its first connection.",
        "Ensuite, redémarrez votre site ou programme pour qu'il lise ces lignes. La page de l'application montre sa première connexion.",
        "Después, reinicia tu sitio web o programa para que lea estas líneas. La página de la aplicación muestra su primera conexión.",
    ),
    (
        "Close without copying the token? It cannot be shown again.",
        "Fermer sans avoir copié le jeton ? Il ne pourra plus être affiché.",
        "¿Cerrar sin haber copiado el token? No se podrá volver a mostrar.",
    ),
    ("Back", "Revenir", "Volver"),
    ("Close anyway", "Fermer quand même", "Cerrar de todos modos"),
    ("I have copied it", "Je l'ai copié", "Lo he copiado"),
    (
        "The clipboard is empty again.",
        "Le presse-papiers est de nouveau vide.",
        "El portapapeles vuelve a estar vacío.",
    ),
    ("Clear the clipboard", "Vider le presse-papiers", "Vaciar el portapapeles"),
    (
        "It works: it opens {}, {} ({} ms), tested from this computer through {}.",
        "Il fonctionne : il ouvre {}, en {} ({} ms), testé depuis cet ordinateur via {}.",
        "Funciona: abre {}, con acceso de {} ({} ms), probado desde este equipo a través de {}.",
    ),
    (
        "The server does not recognise this token: make another one.",
        "Le serveur ne reconnaît pas ce jeton : créez-en un autre.",
        "El servidor no reconoce este token: crea otro.",
    ),
    (
        "The test could not reach the server; the token itself is fine. Test again in a moment.",
        "Le test n'a pas pu joindre le serveur ; le jeton lui-même n'est pas en cause. Testez à nouveau dans un instant.",
        "La prueba no pudo llegar al servidor; el token en sí está bien. Vuelve a probar en un momento.",
    ),
    (
        "nuage_{}… of {} will open nothing any more: what uses it (a site, a script) loses access at once. It cannot be undone; make a new token instead.",
        "nuage_{}… de {} n'ouvrira plus rien : ce qui l'utilise (un site, un script) perd l'accès tout de suite. Cela ne s'annule pas ; créez plutôt un nouveau jeton.",
        "nuage_{}… de {} ya no abrirá nada: lo que lo usa (un sitio, un script) pierde el acceso al momento. No se puede deshacer; crea un token nuevo en su lugar.",
    ),
    ("Keep it", "Le garder", "Conservarlo"),
    ("This computer", "Cet ordinateur", "Este equipo"),
    ("Tools", "Outils", "Herramientas"),
    ("About", "À propos", "Acerca de"),
    ("The editor ran into a bug", "L'éditeur a rencontré un bug", "El editor encontró un error de programa"),
    (
        "It stopped what it was showing. A change already sent to the server may or may not have been carried out: check after reconnecting.",
        "Il a arrêté ce qu'il montrait. Un changement déjà envoyé au serveur a pu être fait ou non : vérifiez après vous être reconnecté.",
        "Detuvo lo que mostraba. Puede que un cambio ya enviado al servidor se haya hecho o no: compruébalo al volver a conectarte.",
    ),
    ("Details", "Détails", "Detalles"),
    ("Nuage DB Editor", "Nuage DB Éditeur", "Nuage DB Editor"),
    (
        "The server no longer accepts this token: it was changed on the server. Type the new one.",
        "Le serveur n'accepte plus ce jeton : il a été changé sur le serveur. Tapez le nouveau.",
        "El servidor ya no acepta este token: se cambió en el servidor. Escribe el nuevo.",
    ),
    (
        "Everything the editor kept on this computer is forgotten.",
        "Tout ce que l'éditeur gardait sur cet ordinateur est oublié.",
        "Todo lo que el editor guardaba en este equipo está olvidado.",
    ),
    (
        "The token of {} could not be removed from {}. Remove the entry io.github.maxlestage.nuage-editor yourself in Keychain Access, Credential Manager or your keyring.",
        "Le jeton de {} n'a pas pu être retiré de {}. Retirez vous-même l'entrée io.github.maxlestage.nuage-editor dans Trousseaux d'accès, le Gestionnaire d'identification ou votre trousseau.",
        "No se pudo quitar el token de {} de {}. Quita tú la entrada io.github.maxlestage.nuage-editor en Acceso a Llaveros, el Administrador de credenciales o tu llavero.",
    ),
    (
        "This one looks like an application's token.",
        "Celui-ci ressemble au jeton d'une application.",
        "Este parece el token de una aplicación.",
    ),
    (
        "Read: port {}, management token found, public address {}.",
        "Lu : port {}, jeton de gestion trouvé, adresse publique {}.",
        "Leído: puerto {}, token de gestión encontrado, dirección pública {}.",
    ),
    (
        "Read: port {}, management token found.",
        "Lu : port {}, jeton de gestion trouvé.",
        "Leído: puerto {}, token de gestión encontrado.",
    ),
    (
        "A change is on its way: wait for its answer.",
        "Un changement est en route : attendez sa réponse.",
        "Hay un cambio en camino: espera su respuesta.",
    ),
    ("Open the live dashboard", "Ouvrir le tableau de bord en direct", "Abrir el panel en directo"),
    ("Encrypted (HTTPS)", "Chiffré (HTTPS)", "Cifrado (HTTPS)"),
    ("Not encrypted: this computer only", "Non chiffré : cet ordinateur seulement", "Sin cifrar: solo este equipo"),
    ("Not encrypted: local network", "Non chiffré : réseau local", "Sin cifrar: red local"),
    ("Not encrypted: Internet", "Non chiffré : Internet", "Sin cifrar: Internet"),
    ("Language: as the system ({})", "Langue : comme le système ({})", "Idioma: como el sistema ({})"),
    (
        "No website may call {} from a page any more.",
        "Plus aucun site ne peut appeler {} depuis une page.",
        "Ya ningún sitio puede llamar a {} desde una página.",
    ),
    (
        "Saved: pages of {} may call {} now.",
        "Enregistré : les pages de {} peuvent appeler {} dès maintenant.",
        "Guardado: las páginas de {} ya pueden llamar a {}.",
    ),
    (
        "There is already an application named {}: choose another name.",
        "Il y a déjà une application « {} » : choisissez un autre nom.",
        "Ya hay una aplicación llamada {}: elige otro nombre.",
    ),
    ("Not created:", "Non créée :", "No creada:"),
    ("This token was already revoked.", "Ce jeton était déjà révoqué.", "Este token ya estaba revocado."),
    (
        "Detail from the server (in English): {}",
        "Détail donné par le serveur (en anglais) : {}",
        "Detalle del servidor (en inglés): {}",
    ),
    ("Saving…", "Enregistrement…", "Guardando…"),
    (
        "{} ({} s; the editor waits up to {} s)",
        "{} ({} s ; l'éditeur attend jusqu'à {} s)",
        "{} ({} s; el editor espera hasta {} s)",
    ),
    ("Could not refresh: {}", "Actualisation impossible : {}", "No se pudo actualizar: {}"),
    ("What is shown is as of {}.", "Ce qui est affiché date de {}.", "Lo que se muestra es de las {}."),
    ("Try again", "Réessayer", "Reintentar"),
    (
        "You cancelled: nothing was read. Try again, or paste the token yourself.",
        "Vous avez annulé : rien n'a été lu. Réessayez, ou collez le jeton vous-même.",
        "Has cancelado: no se ha leído nada. Vuelve a intentarlo, o pega el token tú mismo.",
    ),
    (
        "The password was not accepted, or this account cannot act as administrator ({}). If someone else installed Nuage DB, ask them to press Continue once in your session.",
        "Le mot de passe n'a pas été accepté, ou ce compte ne peut pas agir en administrateur ({}). Si quelqu'un d'autre a installé Nuage DB, demandez-lui d'appuyer une fois sur Continuer dans votre session.",
        "No se aceptó la contraseña, o esta cuenta no puede actuar como administrador ({}). Si otra persona instaló Nuage DB, pídele que pulse una vez Continuar en tu sesión.",
    ),
    (
        "This desktop cannot show a password window ({}): paste the token instead; it is the line NUAGE_ADMIN_TOKEN= of the settings file.",
        "Ce bureau ne peut pas afficher de fenêtre de mot de passe ({}) : collez plutôt le jeton ; c'est la ligne NUAGE_ADMIN_TOKEN= du fichier de réglages.",
        "Este escritorio no puede mostrar una ventana de contraseña ({}): pega el token en su lugar; es la línea NUAGE_ADMIN_TOKEN= del archivo de ajustes.",
    ),
    (
        "The settings file could not be read: {}.",
        "Le fichier de réglages n'a pas pu être lu : {}.",
        "No se pudo leer el archivo de ajustes: {}.",
    ),
    (
        "Windows did not hand the file over (code {}). Try again; if it fails again, paste the token.",
        "Windows n'a pas transmis le fichier (code {}). Réessayez ; si cela échoue encore, collez le jeton.",
        "Windows no entregó el archivo (código {}). Vuelve a intentarlo; si falla otra vez, pega el token.",
    ),
    (
        "Windows did not hand the file over. Try again; if it fails again, paste the token.",
        "Windows n'a pas transmis le fichier. Réessayez ; si cela échoue encore, collez le jeton.",
        "Windows no entregó el archivo. Vuelve a intentarlo; si falla otra vez, pega el token.",
    ),
    (
        "This server has no management token: its settings file ({}) has no line NUAGE_ADMIN_TOKEN= (Linux servers installed before version 0.4.2). An administrator adds one, with a token from Tools, Generate a token, then restarts the service.",
        "Ce serveur n'a pas de jeton de gestion : son fichier de réglages ({}) n'a pas de ligne NUAGE_ADMIN_TOKEN= (serveurs Linux installés avant la version 0.4.2). Un administrateur en ajoute une, avec un jeton donné par Outils, Générer un jeton, puis redémarre le service.",
        "Este servidor no tiene token de gestión: su archivo de ajustes ({}) no tiene línea NUAGE_ADMIN_TOKEN= (servidores Linux instalados antes de la versión 0.4.2). Un administrador añade una, con un token de Herramientas, Generar un token, y reinicia el servicio.",
    ),
    (
        "Saved and in force, but not yet in the backup: a restart of the server now would bring the earlier value back.",
        "Enregistré et en vigueur, mais pas encore dans la sauvegarde : un redémarrage du serveur maintenant ferait revenir l'ancienne valeur.",
        "Guardado y en vigor, pero aún no en la copia de seguridad: un reinicio del servidor ahora traería de vuelta el valor anterior.",
    ),
    (
        "This version of Nuage DB changes the websites in the web console only. The console asks for its password (NUAGE_ADMIN_PASSWORD).",
        "Cette version de Nuage DB ne change les sites web que dans la console web. La console demande son mot de passe (NUAGE_ADMIN_PASSWORD).",
        "Esta versión de Nuage DB solo cambia los sitios web en la consola web. La consola pide su contraseña (NUAGE_ADMIN_PASSWORD).",
    ),
    (
        "Nuage DB keeps its settings in a file only administrators can read (/etc/nuage/nuage.env): it holds the encryption key. Your computer will now ask for your password.",
        "Nuage DB garde ses réglages dans un fichier que seuls les administrateurs peuvent lire (/etc/nuage/nuage.env) : il contient la clé de chiffrement. Votre ordinateur va vous demander votre mot de passe.",
        "Nuage DB guarda sus ajustes en un archivo que solo los administradores pueden leer (/etc/nuage/nuage.env): contiene la clave de cifrado. Tu equipo te va a pedir tu contraseña.",
    ),
    (
        "Nuage DB keeps its settings in a file only administrators can read (/usr/local/etc/nuage/nuage.env): it holds the encryption key. This Mac will now ask for an administrator's name and password.",
        "Nuage DB garde ses réglages dans un fichier que seuls les administrateurs peuvent lire (/usr/local/etc/nuage/nuage.env) : il contient la clé de chiffrement. Ce Mac va demander le nom et le mot de passe d'un administrateur.",
        "Nuage DB guarda sus ajustes en un archivo que solo los administradores pueden leer (/usr/local/etc/nuage/nuage.env): contiene la clave de cifrado. Este Mac va a pedir el nombre y la contraseña de un administrador.",
    ),
    (
        "Nuage DB keeps its settings in a file only administrators can read (C:\\ProgramData\\Nuage\\nuage.env). Windows will now ask for permission. Its window names the Windows Command Processor, published by Microsoft: that is expected, the editor uses it to read four lines of this file, and changes nothing.",
        "Nuage DB garde ses réglages dans un fichier que seuls les administrateurs peuvent lire (C:\\ProgramData\\Nuage\\nuage.env). Windows va vous demander une autorisation : sa fenêtre parle du « Processeur de commandes Windows », publié par Microsoft. C'est normal : l'éditeur s'en sert pour lire quatre lignes de ce fichier, et ne modifie rien.",
        "Nuage DB guarda sus ajustes en un archivo que solo los administradores pueden leer (C:\\ProgramData\\Nuage\\nuage.env). Windows te va a pedir permiso: su ventana nombra el «Procesador de comandos de Windows», publicado por Microsoft. Es normal: el editor lo usa para leer cuatro líneas de este archivo, y no cambia nada.",
    ),
    (
        "Copied. It stays in the clipboard until you copy something else; the clipboard history is asked to skip it. On Linux, what you copy may disappear when the editor closes: paste it first.",
        "Copié. Il reste dans le presse-papiers jusqu'à la prochaine copie ; l'historique du presse-papiers est prié de l'ignorer. Sous Linux, ce que vous copiez peut disparaître à la fermeture de l'éditeur : collez-le avant.",
        "Copiado. Se queda en el portapapeles hasta que copies otra cosa; se pide al historial del portapapeles que lo ignore. En Linux, lo que copias puede desaparecer al cerrar el editor: pégalo antes.",
    ),
    ("Nuage DB answers", "Nuage DB répond", "Nuage DB responde"),
    (
        "Newer versions of Nuage DB tell the editor their version, their memory and whether the last backup worked: update the server to see it here.",
        "Les versions plus récentes de Nuage DB disent à l'éditeur leur version, leur mémoire et si la dernière sauvegarde a fonctionné : mettez le serveur à jour pour le voir ici.",
        "Las versiones más recientes de Nuage DB le dicen al editor su versión, su memoria y si la última copia de seguridad funcionó: actualiza el servidor para verlo aquí.",
    ),
    ("See them", "Les voir", "Verlas"),
    (
        "This server does not say its memory to the editor.",
        "Ce serveur ne dit pas sa mémoire à l'éditeur.",
        "Este servidor no le dice su memoria al editor.",
    ),
    ("Change", "Régler", "Ajustar"),
    ("How to change where they go", "Changer de destination", "Cambiar el destino"),
    (
        "The destination is chosen when Nuage DB is installed: run the installer again with another destination (the guide shows the line). On Heroku: Settings, then Config Vars, the NUAGE_BACKUP_S3_* variables. The server restarts and keeps its data.",
        "La destination se choisit à l'installation de Nuage DB : relancez l'installateur avec une autre destination (le guide montre la ligne). Sur Heroku : Settings, puis Config Vars, les variables NUAGE_BACKUP_S3_*. Le serveur redémarre et garde ses données.",
        "El destino se elige al instalar Nuage DB: vuelve a ejecutar el instalador con otro destino (la guía muestra la línea). En Heroku: Settings, luego Config Vars, las variables NUAGE_BACKUP_S3_*. El servidor se reinicia y conserva sus datos.",
    ),
    ("Open the guide", "Ouvrir le guide", "Abrir la guía"),
    ("Updated {}", "Mis à jour {}", "Actualizado {}"),
    ("running for {}", "en marche depuis {}", "en marcha desde hace {}"),
    ("version {}", "version {}", "versión {}"),
    (
        "everything it stores is encrypted ({})",
        "tout ce qu'il stocke est chiffré ({})",
        "todo lo que guarda está cifrado ({})",
    ),
    (
        "its data is not encrypted on its disk",
        "ses données ne sont pas chiffrées sur son disque",
        "sus datos no están cifrados en su disco",
    ),
    ("{} application", "{} application", "{} aplicación"),
    ("{} applications", "{} applications", "{} aplicaciones"),
    ("{} · last connection {} ({})", "{} · dernière connexion {} ({})", "{} · última conexión {} ({})"),
    (
        "Nuage DB uses {} · this server has {}, {} free",
        "Nuage DB occupe {} · ce serveur a {}, dont {} libres",
        "Nuage DB ocupa {} · este servidor tiene {}, de los que {} están libres",
    ),
    (
        "No backup: if this server's disk fails, its data is lost.",
        "Aucune sauvegarde : si le disque de ce serveur lâche, ses données sont perdues.",
        "Ninguna copia de seguridad: si el disco de este servidor falla, sus datos se pierden.",
    ),
    (
        "On Heroku the disk is emptied at every restart, at least once a day.",
        "Sur Heroku, le disque est vidé à chaque redémarrage, au moins une fois par jour.",
        "En Heroku, el disco se vacía en cada reinicio, al menos una vez al día.",
    ),
    ("Backups go to {}.", "Les sauvegardes vont vers {}.", "Las copias de seguridad van a {}."),
    (
        "Newer versions of Nuage DB tell the editor whether the last copy worked: update the server to see it here.",
        "Les versions plus récentes de Nuage DB disent à l'éditeur si la dernière copie a fonctionné : mettez le serveur à jour pour le voir ici.",
        "Las versiones más recientes de Nuage DB le dicen al editor si la última copia funcionó: actualiza el servidor para verlo aquí.",
    ),
    (
        "The last copy failed: {}. The data is safe on the server's disk, but not copied elsewhere.",
        "La dernière copie a échoué : {}. Les données sont sur le disque du serveur, mais pas copiées ailleurs.",
        "La última copia falló: {}. Los datos están a salvo en el disco del servidor, pero no copiados en otro sitio.",
    ),
    ("Last copy {}, to {}", "Dernière copie {}, vers {}", "Última copia {}, a {}"),
    (
        "Waiting for the first copy, to {}",
        "En attente de la première copie, vers {}",
        "Esperando la primera copia, a {}",
    ),
    (
        "The settings of this server are those of its environment (NUAGE_SETTINGS=environment): the editor shows them and cannot change them.",
        "Les réglages de ce serveur sont ceux de son environnement (NUAGE_SETTINGS=environment) : l'éditeur les montre et ne peut pas les changer.",
        "Los ajustes de este servidor son los de su entorno (NUAGE_SETTINGS=environment): el editor los muestra y no puede cambiarlos.",
    ),
    ("Reading the settings…", "Lecture des réglages…", "Leyendo los ajustes…"),
    (
        "Nuage DB uses {} now. This server has {}, {} of it free.",
        "Nuage DB occupe {} en ce moment. Ce serveur a {}, dont {} libres.",
        "Nuage DB ocupa {} ahora. Este servidor tiene {}, de los que {} están libres.",
    ),
    (
        "Amounts are written auto, 512 MiB or 2 GiB (a number alone is in MiB). In force at once, with no restart.",
        "Les quantités s'écrivent auto, 512 Mio ou 2 Gio (un nombre seul est en Mio). En vigueur tout de suite, sans redémarrage.",
        "Las cantidades se escriben auto, 512 MiB o 2 GiB (un número solo va en MiB). En vigor al momento, sin reiniciar.",
    ),
    (
        "In force at once, with no restart.",
        "En vigueur tout de suite, sans redémarrage.",
        "En vigor al momento, sin reiniciar.",
    ),
    (
        "Each has a thread; the next ones are refused as PostgreSQL does. Between 1 and 10 000.",
        "Chacune a un thread ; les suivantes sont refusées, comme le fait PostgreSQL. Entre 1 et 10 000.",
        "Cada una tiene un hilo; las siguientes se rechazan, como hace PostgreSQL. Entre 1 y 10 000.",
    ),
    (
        "PostgreSQL connections at once: {} is not between 1 and 10 000.",
        "Connexions PostgreSQL simultanées : {} n'est pas entre 1 et 10 000.",
        "Conexiones PostgreSQL simultáneas: {} no está entre 1 y 10 000.",
    ),
    (
        "Public status: /api/status and the dashboard show a public subset without signing in",
        "État public : /api/status et le tableau de bord montrent l'essentiel sans connexion",
        "Estado público: /api/status y el panel muestran una parte pública sin iniciar sesión",
    ),
    (
        "This version does not list them: the console's Server page does.",
        "Cette version ne les liste pas : la page Serveur de la console, si.",
        "Esta versión no los lista: la página Servidor de la consola, sí.",
    ),
    (
        "The server reads these before it opens its data, or they are what it listens on and what opens it: a wrong value could lock you out, so they are not changed from here.",
        "Le serveur les lit avant d'ouvrir ses données, ou bien ce sont ses ports et ce qui lui donne accès : une mauvaise valeur pourrait vous en fermer la porte, alors ils ne se changent pas d'ici.",
        "El servidor los lee antes de abrir sus datos, o son sus puertos y lo que da acceso a él: un valor erróneo podría dejarte fuera, así que no se cambian desde aquí.",
    ),
    (
        "On the server's computer: its settings file {}, then a restart of its service.",
        "Sur l'ordinateur du serveur : son fichier de réglages {}, puis un redémarrage de son service.",
        "En el equipo del servidor: su archivo de ajustes {}, y luego un reinicio de su servicio.",
    ),
    (
        "On Heroku: Settings, then Config Vars; Heroku restarts the server by itself.",
        "Sur Heroku : Settings, puis Config Vars ; Heroku redémarre le serveur tout seul.",
        "En Heroku: Settings, luego Config Vars; Heroku reinicia el servidor por su cuenta.",
    ),
    (
        "In the server's environment (a host's variables, Docker's .env), then a restart.",
        "Dans l'environnement du serveur (les variables d'un hébergeur, le .env de Docker), puis un redémarrage.",
        "En el entorno del servidor (las variables de un alojamiento, el .env de Docker), y luego un reinicio.",
    ),
    ("not set", "non réglé", "no definido"),
    ("Show the ones not set", "Montrer ceux qui ne sont pas réglés", "Mostrar los que no están definidos"),
    ("Welcome", "Bienvenue", "Bienvenido"),
    (
        "This editor looks after your Nuage DB server without a terminal: see that it works, create an application for your website, choose how much memory it uses.",
        "Cet éditeur s'occupe de votre serveur Nuage DB sans terminal : voir qu'il fonctionne, créer une application pour votre site, choisir la mémoire qu'il utilise.",
        "Este editor se ocupa de tu servidor Nuage DB sin terminal: ver que funciona, crear una aplicación para tu sitio web, elegir cuánta memoria usa.",
    ),
    (
        "The editor changes nothing by itself: every change is a button you press, and it says what it did.",
        "L'éditeur ne change rien de lui-même : chaque changement est un bouton que vous pressez, et il dit ce qu'il a fait.",
        "El editor no cambia nada por su cuenta: cada cambio es un botón que pulsas, y dice lo que ha hecho.",
    ),
    ("Connecting", "Connexion", "Conexión en curso"),
    ("Looking for the server…", "Recherche du serveur…", "Buscando el servidor…"),
    ("Checking the token…", "Vérification du jeton…", "Comprobando el token…"),
    ("Change the address or the token", "Changer l'adresse ou le jeton", "Cambiar la dirección o el token"),
    (
        "Stopped waiting. The server may still answer later; nothing was changed.",
        "Attente arrêtée. Le serveur répondra peut-être plus tard ; rien n'a été changé.",
        "Se dejó de esperar. Puede que el servidor responda más tarde; no se ha cambiado nada.",
    ),
    (
        "Read the management token of Nuage DB",
        "Lire le jeton de gestion de Nuage DB",
        "Leer el token de gestión de Nuage DB",
    ),
    (
        "Only four lines are read: NUAGE_ADMIN_TOKEN, PORT, NUAGE_BIND and SITE_URL. The encryption key and the passwords are not read.",
        "Seules quatre lignes sont lues : NUAGE_ADMIN_TOKEN, PORT, NUAGE_BIND et SITE_URL. La clé de chiffrement et les mots de passe ne sont pas lus.",
        "Solo se leen cuatro líneas: NUAGE_ADMIN_TOKEN, PORT, NUAGE_BIND y SITE_URL. La clave de cifrado y las contraseñas no se leen.",
    ),
    (
        "This desktop cannot show a password window ({}): paste the token instead; it is the line NUAGE_ADMIN_TOKEN= of {}.",
        "Ce bureau ne peut pas afficher de fenêtre de mot de passe ({}) : collez plutôt le jeton ; c'est la ligne NUAGE_ADMIN_TOKEN= de {}.",
        "Este escritorio no puede mostrar una ventana de contraseña ({}): pega el token en su lugar; es la línea NUAGE_ADMIN_TOKEN= de {}.",
    ),
    (
        "Waiting for your answer in the system's window…",
        "En attente de votre réponse dans la fenêtre du système…",
        "Esperando tu respuesta en la ventana del sistema…",
    ),
    (
        "Windows' window cannot be withdrawn from here: answer it, or close it.",
        "La fenêtre de Windows ne peut pas être retirée d'ici : répondez-y, ou fermez-la.",
        "La ventana de Windows no se puede retirar desde aquí: respóndela o ciérrala.",
    ),
    ("Continue", "Continuer", "Continuar"),
    ("I have the token, I'll paste it", "J'ai le jeton, je le colle", "Tengo el token, lo pego"),
    (
        "Nuage DB on this computer — token kept in {}",
        "Nuage DB sur cet ordinateur — jeton gardé dans {}",
        "Nuage DB en este equipo: token guardado en {}",
    ),
    ("Open", "Ouvrir", "Abrir"),
    (
        "Nuage DB is installed on this computer.",
        "Nuage DB est installé sur cet ordinateur.",
        "Nuage DB está instalado en este equipo.",
    ),
    ("Nothing to type.", "Rien à taper.", "Nada que escribir."),
    ("Open…", "Ouvrir…", "Abrir…"),
    (
        "Nuage DB seems installed, but its service is missing: installing it again repairs it; data and settings stay.",
        "Nuage DB semble installé, mais son service manque : le réinstaller le répare ; données et réglages restent.",
        "Nuage DB parece instalado, pero falta su servicio: volver a instalarlo lo repara; los datos y los ajustes se quedan.",
    ),
    ("Answers on port {}", "Répond sur le port {}", "Responde en el puerto {}"),
    ("This computer…", "Cet ordinateur…", "Este equipo…"),
    (
        "Nuage DB answers on this computer (port 8080): Docker, or a server started by hand.",
        "Nuage DB répond sur cet ordinateur (port 8080) : Docker, ou un serveur lancé à la main.",
        "Nuage DB responde en este equipo (puerto 8080): Docker, o un servidor arrancado a mano.",
    ),
    ("Use this address", "Utiliser cette adresse", "Usar esta dirección"),
    (
        "My server is elsewhere: Heroku, a host, another computer",
        "Mon serveur est ailleurs : Heroku, un hébergeur, un autre ordinateur",
        "Mi servidor está en otro sitio: Heroku, un alojamiento, otro equipo",
    ),
    ("Server address", "Adresse du serveur", "Dirección del servidor"),
    (
        "The address you type to open the console, without /admin.",
        "L'adresse que vous tapez pour ouvrir la console, sans /admin.",
        "La dirección que escribes para abrir la consola, sin /admin.",
    ),
    (
        "The value of NUAGE_ADMIN_TOKEN on the server (64 characters when the installer or Heroku made it). Not the console password, not an application's token (nuage_…).",
        "La valeur de NUAGE_ADMIN_TOKEN sur le serveur (64 caractères quand l'installateur ou Heroku l'a créé). Ni le mot de passe de la console, ni le jeton d'une application (nuage_…).",
        "El valor de NUAGE_ADMIN_TOKEN en el servidor (64 caracteres si lo creó el instalador o Heroku). Ni la contraseña de la consola, ni el token de una aplicación (nuage_…).",
    ),
    ("Where do I find it?", "Où le trouver ?", "¿Dónde lo encuentro?"),
    ("Installed with the installer", "Installé avec l'installateur", "Instalado con el instalador"),
    (
        "1. Open your app in the Heroku dashboard. 2. Settings, then Reveal Config Vars. 3. Copy the value of NUAGE_ADMIN_TOKEN.",
        "1. Ouvrez votre app dans le tableau de bord Heroku. 2. Settings, puis Reveal Config Vars. 3. Copiez la valeur de NUAGE_ADMIN_TOKEN.",
        "1. Abre tu app en el panel de Heroku. 2. Settings, luego Reveal Config Vars. 3. Copia el valor de NUAGE_ADMIN_TOKEN.",
    ),
    ("Open the Heroku dashboard", "Ouvrir le tableau de bord Heroku", "Abrir el panel de Heroku"),
    (
        "No NUAGE_ADMIN_TOKEN there (a deploy from before 0.4.2)? Add one: name NUAGE_ADMIN_TOKEN, value from Generate a token. Heroku restarts the server by itself (about a minute).",
        "Pas de NUAGE_ADMIN_TOKEN (un déploiement d'avant 0.4.2) ? Ajoutez-en un : nom NUAGE_ADMIN_TOKEN, valeur donnée par Générer un jeton. Heroku redémarre le serveur tout seul (environ une minute).",
        "¿No hay NUAGE_ADMIN_TOKEN (un despliegue anterior a 0.4.2)? Añade uno: nombre NUAGE_ADMIN_TOKEN, valor dado por Generar un token. Heroku reinicia el servidor por su cuenta (alrededor de un minuto).",
    ),
    (
        "On the server's computer, in its settings file, the line NUAGE_ADMIN_TOKEN= (Linux /etc/nuage/nuage.env, Mac /usr/local/etc/nuage/nuage.env, Windows C:\\ProgramData\\Nuage\\nuage.env); only an administrator of that computer can open it. Simplest: open the editor on that computer once.",
        "Sur l'ordinateur du serveur, dans son fichier de réglages, la ligne NUAGE_ADMIN_TOKEN= (Linux /etc/nuage/nuage.env, Mac /usr/local/etc/nuage/nuage.env, Windows C:\\ProgramData\\Nuage\\nuage.env) ; seul un administrateur de cet ordinateur peut l'ouvrir. Le plus simple : ouvrir une fois l'éditeur sur cet ordinateur.",
        "En el equipo del servidor, en su archivo de ajustes, la línea NUAGE_ADMIN_TOKEN= (Linux /etc/nuage/nuage.env, Mac /usr/local/etc/nuage/nuage.env, Windows C:\\ProgramData\\Nuage\\nuage.env); solo un administrador de ese equipo puede abrirlo. Lo más sencillo: abrir una vez el editor en ese equipo.",
    ),
    (
        "In the .env file next to docker-compose.yml, the line NUAGE_ADMIN_TOKEN=. Without one: Generate a token, add the line, then restart the container.",
        "Dans le fichier .env à côté de docker-compose.yml, la ligne NUAGE_ADMIN_TOKEN=. S'il n'y en a pas : Générer un jeton, ajoutez la ligne, puis redémarrez le conteneur.",
        "En el archivo .env junto a docker-compose.yml, la línea NUAGE_ADMIN_TOKEN=. Si no la hay: Generar un token, añade la línea y reinicia el contenedor.",
    ),
    ("read from this computer", "lu sur cet ordinateur", "leído en este equipo"),
    ("Forget this server?", "Oublier ce serveur ?", "¿Olvidar este servidor?"),
    (
        "Forget {}? Its kept token is deleted from {}.",
        "Oublier {} ? Son jeton gardé est supprimé de {}.",
        "¿Olvidar {}? Su token guardado se borra de {}.",
    ),
    ("Forget {}?", "Oublier {} ?", "¿Olvidar {}?"),
    ("Forget everything on this computer?", "Tout oublier sur cet ordinateur ?", "¿Olvidarlo todo en este equipo?"),
    (
        "The servers you used and the language and theme chosen are forgotten, and every token the editor kept is deleted from {}. Your servers are not changed.",
        "Les serveurs utilisés, la langue et le thème choisis sont oubliés, et chaque jeton gardé par l'éditeur est supprimé de {}. Vos serveurs ne sont pas modifiés.",
        "Se olvidan los servidores usados, el idioma y el tema elegidos, y cada token guardado por el editor se borra de {}. Tus servidores no se modifican.",
    ),
    ("Forget everything", "Tout oublier", "Olvidarlo todo"),
    (
        "A change is being sent to {}. If you quit now it may or may not be carried out.",
        "Un changement est en cours d'envoi à {}. Si vous quittez maintenant, il sera peut-être fait, peut-être pas.",
        "Se está enviando un cambio a {}. Si sales ahora, puede que se haga o puede que no.",
    ),
    (
        "A new token is shown and has not been copied: it cannot be shown again.",
        "Un nouveau jeton est affiché et n'a pas été copié : il ne pourra plus être affiché.",
        "Se muestra un token nuevo y no se ha copiado: no se podrá volver a mostrar.",
    ),
    ("Wait", "Attendre", "Esperar"),
    ("Back to it", "Y revenir", "Volver a él"),
    ("Quit anyway", "Quitter quand même", "Salir de todos modos"),
    ("I don't have a server yet", "Je n'ai pas encore de serveur", "Aún no tengo servidor"),
    (
        "Installing Nuage DB on a computer takes one command, pasted once into Terminal (Mac, Linux) or into PowerShell opened as administrator (Windows). Online, the Deploy to Heroku button needs no command.",
        "Installer Nuage DB sur un ordinateur demande une commande, collée une fois dans le Terminal (Mac, Linux) ou dans PowerShell ouvert en administrateur (Windows). En ligne, le bouton Deploy to Heroku n'en demande aucune.",
        "Instalar Nuage DB en un equipo requiere una orden, pegada una vez en el Terminal (Mac, Linux) o en PowerShell abierto como administrador (Windows). En línea, el botón Deploy to Heroku no requiere ninguna.",
    ),
    (
        "Copy the command for this computer",
        "Copier la commande pour cet ordinateur",
        "Copiar la orden para este equipo",
    ),
    ("Open the installation guide", "Ouvrir le guide d'installation", "Abrir la guía de instalación"),
    ("Open Deploy to Heroku", "Ouvrir Deploy to Heroku", "Abrir Deploy to Heroku"),
    ("Then come back here.", "Puis revenez ici.", "Luego vuelve aquí."),
    (
        "Your password will be asked once, to read its management token.",
        "Votre mot de passe vous sera demandé une fois, pour lire son jeton de gestion.",
        "Se te pedirá tu contraseña una vez, para leer su token de gestión.",
    ),
    (
        "This Mac will ask for an administrator's name and password.",
        "Ce Mac demandera le nom et le mot de passe d'un administrateur.",
        "Este Mac pedirá el nombre y la contraseña de un administrador.",
    ),
    (
        "Windows will ask an administrator's permission once, to read its management token.",
        "Windows demandera une fois l'autorisation d'un administrateur, pour lire son jeton de gestion.",
        "Windows pedirá una vez el permiso de un administrador, para leer su token de gestión.",
    ),
    ("Generate a token", "Générer un jeton", "Generar un token"),
    ("Generate another", "En générer un autre", "Generar otro"),
    (
        "This system gave no random numbers: {}",
        "Ce système n'a pas donné de nombres aléatoires : {}",
        "Este sistema no ha dado números aleatorios: {}",
    ),
    ("Nuage DB answers.", "Nuage DB répond.", "Nuage DB responde."),
    (
        "It did not start within 60 s. Its log says why: {}. Restarting the computer starts it too.",
        "Il n'a pas démarré en 60 s. Son journal dit pourquoi : {}. Redémarrer l'ordinateur le démarre aussi.",
        "No arrancó en 60 s. Su registro dice por qué: {}. Reiniciar el equipo también lo arranca.",
    ),
    ("Its log: {}.", "Son journal : {}.", "Su registro: {}."),
    (
        "Test an application's connection",
        "Tester la connexion d'une application",
        "Probar la conexión de una aplicación",
    ),
    (
        "What a website or a program does: it opens Nuage DB with an application's token.",
        "Ce que fait un site ou un programme : il ouvre Nuage DB avec le jeton d'une application.",
        "Lo que hace un sitio web o un programa: abre Nuage DB con el token de una aplicación.",
    ),
    ("Application token", "Jeton de l'application", "Token de la aplicación"),
    ("Test", "Tester", "Probar"),
    (
        "Connected to Nuage DB {}: application {}, {}, branch {}, {} table(s) ({} ms).",
        "Connecté à Nuage DB {} : application {}, {}, branche {}, {} table(s) ({} ms).",
        "Conectado a Nuage DB {}: aplicación {}, {}, rama {}, {} tabla(s) ({} ms).",
    ),
    (
        "The server answered, but refused this token.",
        "Le serveur a répondu, mais a refusé ce jeton.",
        "El servidor respondió, pero rechazó este token.",
    ),
    (
        "Not a Nuage DB server of version 0.4 or later, or not this address.",
        "Ce n'est pas un serveur Nuage DB de version 0.4 ou plus récente, ou pas à cette adresse.",
        "No es un servidor Nuage DB de la versión 0.4 o posterior, o no en esta dirección.",
    ),
    (
        "For NUAGE_ADMIN_TOKEN (Heroku Config Vars, Docker .env), or the NUAGE_ENCRYPTION_KEY of a new server. Never replace the NUAGE_ENCRYPTION_KEY of a server that already has data: its data and backups could no longer be read.",
        "Pour NUAGE_ADMIN_TOKEN (Config Vars de Heroku, .env de Docker), ou la NUAGE_ENCRYPTION_KEY d'un nouveau serveur. Ne remplacez jamais la NUAGE_ENCRYPTION_KEY d'un serveur qui a déjà des données : ses données et ses sauvegardes ne pourraient plus être lues.",
        "Para NUAGE_ADMIN_TOKEN (Config Vars de Heroku, .env de Docker), o la NUAGE_ENCRYPTION_KEY de un servidor nuevo. Nunca sustituyas la NUAGE_ENCRYPTION_KEY de un servidor que ya tiene datos: sus datos y sus copias de seguridad ya no se podrían leer.",
    ),
    ("Preferences", "Préférences", "Preferencias"),
    ("Text size", "Taille du texte", "Tamaño del texto"),
    (
        "Ctrl + and Ctrl − change it too, until the editor closes.",
        "Ctrl + et Ctrl − la changent aussi, jusqu'à la fermeture de l'éditeur.",
        "Ctrl + y Ctrl − también lo cambian, hasta que se cierre el editor.",
    ),
    ("What the editor keeps", "Ce que l'éditeur garde", "Lo que guarda el editor"),
    (
        "Nuage DB Editor {}. It keeps no data of its own besides your preferences, and sends nothing to anyone but your servers.",
        "Nuage DB Éditeur {}. Il ne garde aucune donnée à lui en dehors de vos préférences, et n'envoie rien à personne d'autre que vos serveurs.",
        "Nuage DB Editor {}. No guarda ningún dato propio aparte de tus preferencias, y no envía nada a nadie más que a tus servidores.",
    ),
    (
        "Your preferences are kept in {} (never a token).",
        "Vos préférences sont gardées dans {} (jamais un jeton).",
        "Tus preferencias se guardan en {} (nunca un token).",
    ),
    (
        "Your preferences are kept until the editor closes (never a token).",
        "Vos préférences sont gardées jusqu'à la fermeture de l'éditeur (jamais un jeton).",
        "Tus preferencias se guardan hasta que se cierre el editor (nunca un token).",
    ),
    ("Kept tokens: {}, in {}.", "Jetons gardés : {}, dans {}.", "Tokens guardados: {}, en {}."),
    ("Forget everything on this computer", "Tout oublier sur cet ordinateur", "Olvidarlo todo en este equipo"),
    (
        "Add the editor to the applications menu",
        "Ajouter l'éditeur au menu des applications",
        "Añadir el editor al menú de aplicaciones",
    ),
    (
        "Added: {}. If you move the editor, press it again.",
        "Ajouté : {}. Si vous déplacez l'éditeur, appuyez à nouveau.",
        "Añadido: {}. Si mueves el editor, vuelve a pulsarlo.",
    ),
    ("Open the license", "Ouvrir la licence", "Abrir la licencia"),
    ("Nuage DB on this computer", "Nuage DB sur cet ordinateur", "Nuage DB en este equipo"),
    (
        "This computer has no systemd: the installer left Nuage DB to be started by hand.",
        "Cet ordinateur n'a pas systemd : l'installateur a laissé Nuage DB à démarrer à la main.",
        "Este equipo no tiene systemd: el instalador dejó Nuage DB para arrancarlo a mano.",
    ),
    (
        "Starts with the computer: systemd service nuage.",
        "Démarre avec l'ordinateur : service systemd nuage.",
        "Arranca con el equipo: servicio systemd nuage.",
    ),
    (
        "Starts with the computer: launchd daemon io.github.maxlestage.nuage.",
        "Démarre avec l'ordinateur : démon launchd io.github.maxlestage.nuage.",
        "Arranca con el equipo: demonio launchd io.github.maxlestage.nuage.",
    ),
    (
        "Starts with the computer: scheduled task Nuage DB.",
        "Démarre avec l'ordinateur : tâche planifiée Nuage DB.",
        "Arranca con el equipo: tarea programada Nuage DB.",
    ),
    ("Does not answer on port {}", "Ne répond pas sur le port {}", "No responde en el puerto {}"),
    ("Check again", "Vérifier à nouveau", "Volver a comprobar"),
    ("Settings file", "Fichier de réglages", "Archivo de ajustes"),
    ("Log", "Journal", "Registro"),
    ("Copy · {}", "Copier · {}", "Copiar · {}"),
    (
        "The editor starts the service; it never stops or restarts a running server. Your administrator password will be asked.",
        "L'éditeur démarre le service ; il n'arrête ni ne redémarre jamais un serveur qui tourne. Votre mot de passe administrateur sera demandé.",
        "El editor arranca el servicio; nunca detiene ni reinicia un servidor en marcha. Se te pedirá tu contraseña de administrador.",
    ),
    (
        "Starting… waiting for Nuage DB to answer ({} s; 60 s at most; it restores from its backup if its data folder is empty).",
        "Démarrage… en attente de la réponse de Nuage DB ({} s ; 60 s au plus ; il se restaure depuis sa sauvegarde si son dossier de données est vide).",
        "Arrancando… esperando a que Nuage DB responda ({} s; 60 s como máximo; se restaura desde su copia si su carpeta de datos está vacía).",
    ),
    ("Start Nuage DB", "Démarrer Nuage DB", "Arrancar Nuage DB"),
    (
        "Its log: C:\\ProgramData\\Nuage\\nuage.log, readable by administrators.",
        "Son journal : C:\\ProgramData\\Nuage\\nuage.log, lisible par les administrateurs.",
        "Su registro: C:\\ProgramData\\Nuage\\nuage.log, legible por los administradores.",
    ),
    (
        "Its log is in the system journal, unit nuage (GNOME Logs shows it).",
        "Son journal est dans le journal du système, unité nuage (Journaux de GNOME le montre).",
        "Su registro está en el diario del sistema, unidad nuage (Registros de GNOME lo muestra).",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages() {
        assert_eq!(Lang::of_locale("fr-FR"), Lang::Fr);
        assert_eq!(Lang::of_locale("es_ES.UTF-8"), Lang::Es);
        assert_eq!(Lang::of_locale("de-DE"), Lang::En);
        // The person's order: the first language the editor speaks.
        assert_eq!(Lang::of_locales(["de-DE", "fr-FR"]), Lang::Fr);
        assert_eq!(Lang::of_locales(["en-FR", "fr-FR"]), Lang::En);
        assert_eq!(Lang::of_locales(["es_MX"]), Lang::Es);
        assert_eq!(Lang::of_locales(["de-DE", "it-IT"]), Lang::En);
        assert_eq!(Lang::of_locales(Vec::<String>::new()), Lang::En);
        assert_eq!(tf(Lang::En, "Connected to {}", &[&"n.example.com"]), "Connected to n.example.com");
        // Every text has the same `{}` in the three languages, none is empty, and no text is twice.
        let mut seen = std::collections::HashSet::new();
        for (en, fr, es) in TEXTS {
            assert!(seen.insert(*en), "twice: {en}");
            assert!(!en.trim().is_empty() && !fr.trim().is_empty() && !es.trim().is_empty(), "empty: {en}");
            let n = en.matches("{}").count();
            assert_eq!((fr.matches("{}").count(), es.matches("{}").count()), (n, n), "{en}");
        }
    }

    /// The string literals passed as the text of `t(…)` and `tf(…)` in `source`.
    fn texts_in(source: &str) -> Vec<String> {
        let chars: Vec<char> = source.chars().collect();
        let mut found = Vec::new();
        let mut i = 0;
        while i + 2 < chars.len() {
            let call = match (chars.get(i), chars.get(i + 1), chars.get(i + 2)) {
                (Some('t'), Some('('), _) => Some(i + 2),
                (Some('t'), Some('f'), Some('(')) => Some(i + 3),
                _ => None,
            };
            let starts = i == 0 || chars.get(i - 1).is_some_and(|c| !(c.is_alphanumeric() || *c == '_'));
            let (Some(mut j), true) = (call, starts) else {
                i += 1;
                continue;
            };
            // The first argument, to the comma at its depth.
            let mut depth = 0usize;
            while let Some(&c) = chars.get(j) {
                match c {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' if depth == 0 => break,
                    ')' | ']' | '}' => depth -= 1,
                    ',' if depth == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            if chars.get(j) == Some(&',') {
                j += 1;
                while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                    j += 1;
                }
                if chars.get(j) == Some(&'"') {
                    let mut text = String::new();
                    j += 1;
                    while let Some(&c) = chars.get(j) {
                        match c {
                            '"' => break,
                            '\\' => {
                                j += 1;
                                text.push(match chars.get(j) {
                                    Some('n') => '\n',
                                    Some(&other) => other,
                                    None => break,
                                });
                            }
                            c => text.push(c),
                        }
                        j += 1;
                    }
                    found.push(text);
                }
            }
            i = j.max(i + 1);
        }
        found
    }

    fn sources(dir: &std::path::Path, into: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources(&path, into);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let mut source = std::fs::read_to_string(&path).unwrap();
                // These tests' own examples are not texts of the window.
                if path.ends_with("i18n.rs")
                    && let Some(at) = source.find("#[cfg(test)]\nmod tests")
                {
                    source.truncate(at);
                }
                into.push((path.display().to_string(), source));
            }
        }
    }

    #[test]
    fn every_text_of_the_sources_is_translated() {
        let mut files = Vec::new();
        sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut used: Vec<(String, String)> = Vec::new();
        for (file, source) in &files {
            used.extend(texts_in(source).into_iter().map(|text| (file.clone(), text)));
        }
        used.extend(crate::app::CONSTANT_TEXTS.iter().map(|text| ("a constant".to_owned(), (*text).to_owned())));
        assert!(used.len() > 300, "the scan found {} texts", used.len());
        for (file, text) in &used {
            assert!(table().contains_key(text.as_str()), "{file}: no translation for {text:?}");
        }
        // The scan reads what it should.
        assert_eq!(texts_in(r#"x(t(l, "A"), tf(cx.l, "B {}", &[&f(1, 2)]), it(l, "C"), t(l, NAME))"#), ["A", "B {}"]);
        assert_eq!(texts_in("t(\n    l,\n    \"Say \\\"it\\\"\",\n)"), ["Say \"it\""]);
    }

    #[test]
    fn every_character_has_a_glyph() {
        // The fonts the window has (eframe's default fonts, as the tests' and the program's).
        let ctx = eframe::egui::Context::default();
        let mut output = ctx.run_ui(eframe::egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        let mut missing = std::collections::BTreeSet::new();
        // The marks typed in the window's own texts (the masked token, the path of a page).
        let extra = format!("{}›·—−", crate::app::MASK);
        ctx.fonts_mut(|fonts| {
            for text in TEXTS.iter().flat_map(|(en, fr, es)| [*en, *fr, *es]).chain([extra.as_str()]) {
                for c in text.chars().filter(|c| *c != '\n') {
                    for family in [eframe::egui::FontFamily::Proportional, eframe::egui::FontFamily::Monospace] {
                        if !fonts.has_glyph(&eframe::egui::FontId::new(15.0, family.clone()), c) {
                            missing.insert((c, format!("{family:?}")));
                        }
                    }
                }
            }
        });
        assert!(missing.is_empty(), "no glyph for {missing:?}");
    }
}
