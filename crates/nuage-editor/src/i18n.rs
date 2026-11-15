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

    /// The language of this computer.
    pub fn of_system() -> Lang {
        sys_locale::get_locale().map_or(Lang::En, |l| Lang::of_locale(&l))
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
    (
        "Nuage DB Editor stopped unexpectedly (a bug): {}",
        "Nuage DB Éditeur s'est arrêté de façon inattendue (un bug) : {}",
        "Nuage DB Editor se detuvo de forma inesperada (un error del programa): {}",
    ),
    (
        "Nuage DB Editor could not open its window: {}.",
        "Nuage DB Éditeur n'a pas pu ouvrir sa fenêtre : {}.",
        "Nuage DB Editor no pudo abrir su ventana: {}.",
    ),
    (
        "It needs a computer with a screen and graphics (OpenGL 2 or later); on Linux, a desktop (X11 or Wayland).",
        "Il lui faut un ordinateur avec un écran et des graphismes (OpenGL 2 ou plus récent) ; sous Linux, un bureau (X11 ou Wayland).",
        "Necesita un equipo con pantalla y gráficos (OpenGL 2 o posterior); en Linux, un escritorio (X11 o Wayland).",
    ),
    ("This message is also in {}.", "Ce message est aussi dans {}.", "Este mensaje también está en {}."),
    ("Theme", "Thème", "Tema"),
    ("Connection lines", "Lignes de connexion", "Líneas de conexión"),
    ("Language", "Langue", "Idioma"),
    (
        "The token could not be remembered: {}",
        "Le jeton n'a pas pu être retenu : {}",
        "No se pudo recordar el token: {}",
    ),
    (
        "The settings file of this computer has no management token (NUAGE_ADMIN_TOKEN): run the installer again, it adds one.",
        "Le fichier de réglages de cet ordinateur n'a pas de jeton de gestion (NUAGE_ADMIN_TOKEN) : relancez l'installateur, il en ajoute un.",
        "El archivo de ajustes de este equipo no tiene token de gestión (NUAGE_ADMIN_TOKEN): vuelve a ejecutar el instalador, añade uno.",
    ),
    (
        "Saved: in force now, with no restart.",
        "Enregistré : en vigueur tout de suite, sans redémarrage.",
        "Guardado: en vigor al momento, sin reiniciar.",
    ),
    ("Saved and in force, but:", "Enregistré et en vigueur, mais :", "Guardado y en vigor, pero:"),
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
    ("Done.", "C'est fait.", "Hecho."),
    (
        "Type the address of the server first.",
        "Tapez d'abord l'adresse du serveur.",
        "Escribe primero la dirección del servidor.",
    ),
    ("Type its management token first.", "Tapez d'abord son jeton de gestion.", "Escribe primero su token de gestión."),
    ("Editor", "Éditeur", "Editor"),
    ("Connected to {}", "Connecté à {}", "Conectado a {}"),
    ("Not connected", "Non connecté", "Sin conexión"),
    ("Disconnect", "Se déconnecter", "Desconectar"),
    ("Open the console in my browser", "Ouvrir la console dans mon navigateur", "Abrir la consola en mi navegador"),
    ("Refresh", "Actualiser", "Actualizar"),
    ("Connect to your Nuage DB server", "Se connecter à votre serveur Nuage DB", "Conectarse a tu servidor Nuage DB"),
    (
        "The editor changes your server's settings and applications, as the console does, without a terminal. It needs the address of the server and its management token.",
        "L'éditeur change les réglages et les applications de votre serveur, comme la console, sans terminal. Il lui faut l'adresse du serveur et son jeton de gestion.",
        "El editor cambia los ajustes y las aplicaciones de tu servidor, como la consola, sin terminal. Necesita la dirección del servidor y su token de gestión.",
    ),
    ("On this computer", "Sur cet ordinateur", "En este equipo"),
    (
        "A Nuage DB server is installed here (its settings are in {}).",
        "Un serveur Nuage DB est installé ici (ses réglages sont dans {}).",
        "Hay un servidor Nuage DB instalado aquí (sus ajustes están en {}).",
    ),
    (
        "Its settings file is protected (it holds the encryption key): your system asks for your password to read its address and its management token, and nothing else.",
        "Son fichier de réglages est protégé (il contient la clé de chiffrement) : votre système vous demande votre mot de passe pour y lire son adresse et son jeton de gestion, et rien d'autre.",
        "Su archivo de ajustes está protegido (contiene la clave de cifrado): tu sistema te pide tu contraseña para leer su dirección y su token de gestión, y nada más.",
    ),
    ("Enter my password", "Saisir mon mot de passe", "Introducir mi contraseña"),
    (
        "Its settings file is protected (it holds the encryption key). Close the editor, then open it again as administrator (right-click: Run as administrator) to read it once, with \"Remember on this computer\" ticked.",
        "Son fichier de réglages est protégé (il contient la clé de chiffrement). Fermez l'éditeur, puis rouvrez-le en tant qu'administrateur (clic droit : Exécuter en tant qu'administrateur) pour le lire une fois, avec « Retenir sur cet ordinateur » coché.",
        "Su archivo de ajustes está protegido (contiene la clave de cifrado). Cierra el editor y vuelve a abrirlo como administrador (clic derecho: Ejecutar como administrador) para leerlo una vez, con «Recordar en este equipo» marcado.",
    ),
    ("Use this server", "Utiliser ce serveur", "Usar este servidor"),
    (
        "No server was installed on this computer by the installer. If yours is elsewhere (online, another computer), type its address below.",
        "Aucun serveur n'a été installé sur cet ordinateur par l'installateur. Si le vôtre est ailleurs (en ligne, sur un autre ordinateur), tapez son adresse ci-dessous.",
        "El instalador no ha instalado ningún servidor en este equipo. Si el tuyo está en otro sitio (en línea, en otro equipo), escribe su dirección abajo.",
    ),
    ("Address and management token", "Adresse et jeton de gestion", "Dirección y token de gestión"),
    ("Address of the server", "Adresse du serveur", "Dirección del servidor"),
    ("Management token", "Jeton de gestion", "Token de gestión"),
    ("Show", "Afficher", "Mostrar"),
    (
        "The management token is NUAGE_ADMIN_TOKEN: in the settings file the installer wrote, or in your host's variables (Heroku: Settings, Config Vars). It is not an application's token.",
        "Le jeton de gestion, c'est NUAGE_ADMIN_TOKEN : dans le fichier de réglages écrit par l'installateur, ou dans les variables de votre hébergeur (Heroku : Settings, Config Vars). Ce n'est pas le jeton d'une application.",
        "El token de gestión es NUAGE_ADMIN_TOKEN: en el archivo de ajustes que escribió el instalador, o en las variables de tu alojamiento (Heroku: Settings, Config Vars). No es el token de una aplicación.",
    ),
    ("Remember on this computer", "Retenir sur cet ordinateur", "Recordar en este equipo"),
    ("Connect", "Se connecter", "Conectar"),
    ("Checking…", "Vérification…", "Comprobando…"),
    ("Servers you used", "Serveurs déjà utilisés", "Servidores que has usado"),
    ("token remembered", "jeton retenu", "token recordado"),
    ("Forget", "Oublier", "Olvidar"),
    ("Theme: as the system", "Thème : comme le système", "Tema: como el sistema"),
    ("Theme: light", "Thème : clair", "Tema: claro"),
    ("Theme: dark", "Thème : sombre", "Tema: oscuro"),
    ("Language: as the system", "Langue : comme le système", "Idioma: como el sistema"),
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
        "The password was not given: nothing was read. Type the address and the token below instead.",
        "Le mot de passe n'a pas été donné : rien n'a été lu. Tapez plutôt l'adresse et le jeton ci-dessous.",
        "No se dio la contraseña: no se leyó nada. Escribe en su lugar la dirección y el token abajo.",
    ),
    (
        "Its settings file could not be read: {}",
        "Son fichier de réglages n'a pas pu être lu : {}",
        "No se pudo leer su archivo de ajustes: {}",
    ),
    (
        "No server answers at this address. Is it running? Is the address right (with its port, e.g. localhost:8080)?",
        "Aucun serveur ne répond à cette adresse. Est-il démarré ? L'adresse est-elle juste (avec son port, par exemple localhost:8080) ?",
        "Ningún servidor responde en esta dirección. ¿Está en marcha? ¿Es correcta la dirección (con su puerto, por ejemplo localhost:8080)?",
    ),
    (
        "This address does not answer as a Nuage DB server.",
        "Cette adresse ne répond pas comme un serveur Nuage DB.",
        "Esta dirección no responde como un servidor Nuage DB.",
    ),
    (
        "The server refused this token. The editor needs the management token (NUAGE_ADMIN_TOKEN), not an application's token.",
        "Le serveur a refusé ce jeton. L'éditeur a besoin du jeton de gestion (NUAGE_ADMIN_TOKEN), pas du jeton d'une application.",
        "El servidor ha rechazado este token. El editor necesita el token de gestión (NUAGE_ADMIN_TOKEN), no el token de una aplicación.",
    ),
    ("The server refused:", "Le serveur a refusé :", "El servidor ha rechazado:"),
    ("Not saved:", "Non enregistré :", "No guardado:"),
    ("Not found:", "Introuvable :", "No encontrado:"),
    ("It failed:", "Échec :", "Ha fallado:"),
    ("Saving…", "Enregistrement…", "Guardando…"),
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
    (
        "Amounts are written auto, 512 MiB or 2 GiB (a number alone is in MiB).",
        "Les quantités s'écrivent auto, 512 Mio ou 2 Go (un nombre seul est en Mio).",
        "Las cantidades se escriben auto, 512 MiB o 2 GiB (un número solo va en MiB).",
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
    (
        "Each has a thread; the next ones are refused as PostgreSQL does.",
        "Chacune a un thread ; les suivantes sont refusées, comme le fait PostgreSQL.",
        "Cada una tiene un hilo; las siguientes se rechazan, como hace PostgreSQL.",
    ),
    ("Save the limits", "Enregistrer les limites", "Guardar los límites"),
    ("Visibility", "Visibilité", "Visibilidad"),
    (
        "Public status: the dashboard shows a public subset without signing in",
        "État public : le tableau de bord montre une partie publique sans connexion",
        "Estado público: el panel muestra una parte pública sin iniciar sesión",
    ),
    ("New application", "Nouvelle application", "Nueva aplicación"),
    (
        "Each application has its own encrypted database and its own tokens: one per website or program.",
        "Chaque application a sa propre base chiffrée et ses propres jetons : une par site web ou par programme.",
        "Cada aplicación tiene su propia base cifrada y sus propios tokens: una por sitio web o programa.",
    ),
    ("Name", "Nom", "Nombre"),
    ("Description", "Description", "Descripción"),
    ("what this application is", "ce qu'est cette application", "qué es esta aplicación"),
    (
        "A name: lowercase letters, digits, - and _ (1 to 63).",
        "Un nom : lettres minuscules, chiffres, - et _ (1 à 63).",
        "Un nombre: letras minúsculas, cifras, - y _ (de 1 a 63).",
    ),
    ("Create the application", "Créer l'application", "Crear la aplicación"),
    ("the server's own", "celle du serveur", "la del servidor"),
    (
        "Memory: page cache {}, key-value store {}.",
        "Mémoire : cache de pages {}, stockage clé-valeur {}.",
        "Memoria: caché de páginas {}, almacén clave-valor {}.",
    ),
    ("Tokens", "Jetons", "Tokens"),
    (
        "NUAGE_TOKEN (set on the server): read and write.",
        "NUAGE_TOKEN (réglé sur le serveur) : lecture et écriture.",
        "NUAGE_TOKEN (definido en el servidor): lectura y escritura.",
    ),
    (
        "No token: no program can connect yet.",
        "Aucun jeton : aucun programme ne peut encore se connecter.",
        "Ningún token: ningún programa puede conectarse aún.",
    ),
    ("read only", "lecture seule", "solo lectura"),
    ("read and write", "lecture et écriture", "lectura y escritura"),
    ("Revoke", "Révoquer", "Revocar"),
    ("New token (read and write)", "Nouveau jeton (lecture et écriture)", "Nuevo token (lectura y escritura)"),
    ("New read-only token", "Nouveau jeton en lecture seule", "Nuevo token de solo lectura"),
    (
        "A token in a web page is public: give pages a read-only token, and keep read-and-write tokens on servers.",
        "Un jeton dans une page web est public : donnez aux pages un jeton en lecture seule, et gardez les jetons de lecture et écriture sur des serveurs.",
        "Un token en una página web es público: da a las páginas un token de solo lectura, y guarda los de lectura y escritura en servidores.",
    ),
    (
        "Deleting an application (its data and its backups) is done in the console, behind a typed confirmation.",
        "Supprimer une application (ses données et ses sauvegardes) se fait dans la console, après une confirmation tapée.",
        "Eliminar una aplicación (sus datos y sus copias) se hace en la consola, tras una confirmación escrita.",
    ),
    ("The token of {}", "Le jeton de {}", "El token de {}"),
    (
        "Read only: for the pages of a website.",
        "Lecture seule : pour les pages d'un site web.",
        "Solo lectura: para las páginas de un sitio web.",
    ),
    (
        "Read and write: for a server or a program, never in a web page.",
        "Lecture et écriture : pour un serveur ou un programme, jamais dans une page web.",
        "Lectura y escritura: para un servidor o un programa, nunca en una página web.",
    ),
    (
        "Copy it now: it is not shown again (the server keeps only a fingerprint of it).",
        "Copiez-le maintenant : il ne sera plus affiché (le serveur n'en garde qu'une empreinte).",
        "Cópialo ahora: no se vuelve a mostrar (el servidor solo guarda una huella).",
    ),
    ("Copy the two lines", "Copier les deux lignes", "Copiar las dos líneas"),
    ("Copy the token", "Copier le jeton", "Copiar el token"),
    (
        "Give these two lines to the program, as its settings (environment variables).",
        "Donnez ces deux lignes au programme, comme réglages (variables d'environnement).",
        "Da estas dos líneas al programa, como sus ajustes (variables de entorno).",
    ),
    ("I have copied it: close", "Je l'ai copié : fermer", "Lo he copiado: cerrar"),
    ("Revoke this token?", "Révoquer ce jeton ?", "¿Revocar este token?"),
    (
        "nuage_{}… of {} will open nothing any more: the programs that use it lose access at once. It cannot be undone (make a new token instead).",
        "nuage_{}… de {} n'ouvrira plus rien : les programmes qui l'utilisent perdent l'accès aussitôt. C'est définitif (faites plutôt un nouveau jeton).",
        "nuage_{}… de {} ya no abrirá nada: los programas que lo usan pierden el acceso al momento. No se puede deshacer (crea un token nuevo en su lugar).",
    ),
    ("Cancel", "Annuler", "Cancelar"),
    (
        "The server at {} answers, and its management token is right.",
        "Le serveur à l'adresse {} répond, et son jeton de gestion est le bon.",
        "El servidor en {} responde, y su token de gestión es correcto.",
    ),
    ("Memory", "Mémoire", "Memoria"),
    ("{} used, {} free of {}", "{} utilisés, {} libres sur {}", "{} usados, {} libres de {}"),
    ("Page caches", "Caches de pages", "Cachés de páginas"),
    (
        "{} for all the applications (up to {} while memory has room)",
        "{} pour toutes les applications (jusqu'à {} tant que la mémoire le permet)",
        "{} para todas las aplicaciones (hasta {} mientras haya memoria)",
    ),
    ("Applications", "Applications", "Aplicaciones"),
    ("Backups", "Sauvegardes", "Copias de seguridad"),
    (
        "none: if the disk of this server is lost, its data is lost",
        "aucune : si le disque de ce serveur est perdu, ses données le sont aussi",
        "ninguna: si se pierde el disco de este servidor, se pierden sus datos",
    ),
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
    (
        "The server reads these before it opens its data, or they are what it listens on and what opens it: a wrong value could lock you out, so they are not changed from here. They change in the settings file (or the host's variables), then the server restarts.",
        "Le serveur les lit avant d'ouvrir ses données, ou bien ce sont ses ports et ce qui lui donne accès : une mauvaise valeur pourrait vous en fermer la porte, alors ils ne se changent pas d'ici. Ils se changent dans le fichier de réglages (ou les variables de l'hébergeur), puis le serveur redémarre.",
        "El servidor los lee antes de abrir sus datos, o son sus puertos y lo que da acceso a él: un valor erróneo podría dejarte fuera, así que no se cambian desde aquí. Se cambian en el archivo de ajustes (o las variables del alojamiento), y luego el servidor se reinicia.",
    ),
    ("Its settings file: {}", "Son fichier de réglages : {}", "Su archivo de ajustes: {}"),
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
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages() {
        assert_eq!(Lang::of_locale("fr-FR"), Lang::Fr);
        assert_eq!(Lang::of_locale("es_ES.UTF-8"), Lang::Es);
        assert_eq!(Lang::of_locale("de-DE"), Lang::En);
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
