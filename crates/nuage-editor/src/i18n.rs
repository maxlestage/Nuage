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
    ("This message is also in {}.", "Ce message est aussi dans {}.", "Este mensaje también está en {}."),
    ("Theme", "Thème", "Tema"),
    ("Connection lines", "Lignes de connexion", "Líneas de conexión"),
    ("Language", "Langue", "Idioma"),
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
    ("Type its management token first.", "Tapez d'abord son jeton de gestion.", "Escribe primero su token de gestión."),
    (
        "Type the address of the server first.",
        "Tapez d'abord l'adresse du serveur.",
        "Escribe primero la dirección del servidor.",
    ),
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
    ("Enter my password", "Saisir mon mot de passe", "Introducir mi contraseña"),
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
    ("Servers you used", "Serveurs déjà utilisés", "Servidores que has usado"),
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
    ("Checking… ({} s)", "Vérification… ({} s)", "Comprobando… ({} s)"),
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
        "Copied. It stays in the clipboard until you copy something else. Clipboard managers are asked to skip it (KDE's does; some on GNOME do not). On Linux, what you copy may disappear when the editor closes: paste it first.",
        "Copié. Il reste dans le presse-papiers jusqu'à la prochaine copie. Les gestionnaires de presse-papiers sont priés de l'ignorer (celui de KDE le fait ; certains sous GNOME, non). Sous Linux, ce que vous copiez peut disparaître à la fermeture de l'éditeur : collez-le avant.",
        "Copiado. Se queda en el portapapeles hasta que copies otra cosa. Se pide a los gestores del portapapeles que lo ignoren (el de KDE lo hace; algunos de GNOME, no). En Linux, lo que copias puede desaparecer al cerrar el editor: pégalo antes.",
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
    ("Nuage DB {}", "Nuage DB {}", "Nuage DB {}"),
    (
        "Saving… ({} s; the editor waits up to {} s)",
        "Enregistrement… ({} s ; l'éditeur attend jusqu'à {} s)",
        "Guardando… ({} s; el editor espera hasta {} s)",
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
        "This address works for a program on this computer only. A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
        "Cette adresse ne fonctionne que pour un programme de cet ordinateur. Un site hébergé sur Internet ne peut pas joindre un Nuage DB de cet ordinateur : il lui faut une adresse https publique, ou un Nuage DB en ligne, par exemple sur Heroku.",
        "Esta dirección solo sirve para un programa de este equipo. Un sitio alojado en Internet no puede llegar a un Nuage DB de este equipo: necesita una dirección https pública, o un Nuage DB en línea, por ejemplo en Heroku.",
    ),
    (
        "This address works for a program on this computer only. On another device of your network, use http://{}:{} instead (if this computer's firewall lets it in). A website hosted on the Internet cannot reach a Nuage DB on this computer: it needs a public https address, or a Nuage DB online such as on Heroku.",
        "Cette adresse ne fonctionne que pour un programme de cet ordinateur. Sur un autre appareil de votre réseau, utilisez plutôt http://{}:{} (si le pare-feu de cet ordinateur le laisse entrer). Un site hébergé sur Internet ne peut pas joindre un Nuage DB de cet ordinateur : il lui faut une adresse https publique, ou un Nuage DB en ligne, par exemple sur Heroku.",
        "Esta dirección solo sirve para un programa de este equipo. En otro dispositivo de tu red, usa en su lugar http://{}:{} (si el cortafuegos de este equipo lo deja entrar). Un sitio alojado en Internet no puede llegar a un Nuage DB de este equipo: necesita una dirección https pública, o un Nuage DB en línea, por ejemplo en Heroku.",
    ),
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
    ("not encrypted", "non chiffré", "sin cifrar"),
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
    ("The editor hit a bug", "L'éditeur a rencontré un bug", "El editor encontró un error de programa"),
    (
        "Something in the editor went wrong, and its page stopped. Nothing more was sent to your server. What was saved before is saved; a change on its way may or may not have been made: look again once connected.",
        "Quelque chose s'est mal passé dans l'éditeur, et sa page s'est arrêtée. Rien de plus n'a été envoyé à votre serveur. Ce qui était enregistré avant l'est toujours ; un changement en cours a pu être fait ou non : regardez à nouveau une fois connecté.",
        "Algo falló en el editor y su página se detuvo. No se envió nada más a tu servidor. Lo que estaba guardado antes sigue guardado; un cambio en curso puede haberse hecho o no: vuelve a mirar una vez conectado.",
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
    ("Ask Windows", "Demander à Windows", "Pedírselo a Windows"),
    (
        "This system cannot show a window asking for an administrator here ({}). Type the address and the management token below: the token is the NUAGE_ADMIN_TOKEN line of {}.",
        "Ce système ne peut pas afficher ici de fenêtre demandant un administrateur ({}). Tapez l'adresse et le jeton de gestion ci-dessous : le jeton est la ligne NUAGE_ADMIN_TOKEN de {}.",
        "Este sistema no puede mostrar aquí una ventana que pida un administrador ({}). Escribe abajo la dirección y el token de gestión: el token es la línea NUAGE_ADMIN_TOKEN de {}.",
    ),
    (
        "Waiting for the system's window… ({} s)",
        "En attente de la fenêtre du système… ({} s)",
        "Esperando la ventana del sistema… ({} s)",
    ),
    (
        "The window asking for an administrator was closed: nothing was read. Click the button again to be asked once more, or type the address and the token below.",
        "La fenêtre demandant un administrateur a été fermée : rien n'a été lu. Cliquez à nouveau sur le bouton pour qu'elle revienne, ou tapez l'adresse et le jeton ci-dessous.",
        "Se cerró la ventana que pedía un administrador: no se leyó nada. Pulsa otra vez el botón para que vuelva, o escribe abajo la dirección y el token.",
    ),
    (
        "The system did not accept the answer ({}): nothing was read. It takes the password of an administrator of this computer.",
        "Le système n'a pas accepté la réponse ({}) : rien n'a été lu. Il faut le mot de passe d'un administrateur de cet ordinateur.",
        "El sistema no aceptó la respuesta ({}): no se leyó nada. Hace falta la contraseña de un administrador de este equipo.",
    ),
    (
        "This system cannot show a window asking for an administrator here ({}). Type the address and the management token below: the token is the NUAGE_ADMIN_TOKEN line of the settings file.",
        "Ce système ne peut pas afficher ici de fenêtre demandant un administrateur ({}). Tapez l'adresse et le jeton de gestion ci-dessous : le jeton est la ligne NUAGE_ADMIN_TOKEN du fichier de réglages.",
        "Este sistema no puede mostrar aquí una ventana que pida un administrador ({}). Escribe abajo la dirección y el token de gestión: el token es la línea NUAGE_ADMIN_TOKEN del archivo de ajustes.",
    ),
    (
        "The settings file could not be read, even as administrator: {}",
        "Le fichier de réglages n'a pas pu être lu, même en administrateur : {}",
        "No se pudo leer el archivo de ajustes, ni siquiera como administrador: {}",
    ),
    (
        "Windows did not hand the settings file over (code {}): nothing was read.",
        "Windows n'a pas transmis le fichier de réglages (code {}) : rien n'a été lu.",
        "Windows no entregó el archivo de ajustes (código {}): no se leyó nada.",
    ),
    (
        "Windows did not hand the settings file over within two minutes: nothing was read.",
        "Windows n'a pas transmis le fichier de réglages en deux minutes : rien n'a été lu.",
        "Windows no entregó el archivo de ajustes en dos minutos: no se leyó nada.",
    ),
    (
        "restart this computer (the server starts with it)",
        "redémarrez cet ordinateur (le serveur démarre avec lui)",
        "reinicia este equipo (el servidor se inicia con él)",
    ),
    (
        "restart the server: sudo launchctl kickstart -k system/io.github.maxlestage.nuage",
        "redémarrez le serveur : sudo launchctl kickstart -k system/io.github.maxlestage.nuage",
        "reinicia el servidor: sudo launchctl kickstart -k system/io.github.maxlestage.nuage",
    ),
    (
        "restart the server: sudo systemctl restart nuage",
        "redémarrez le serveur : sudo systemctl restart nuage",
        "reinicia el servidor: sudo systemctl restart nuage",
    ),
    (
        "{} has no management token (NUAGE_ADMIN_TOKEN), so the server's management API is off. The installer writes one only when it installs Nuage DB for the first time, not when it updates it: running it again adds none. To add one, as an administrator: add a line NUAGE_ADMIN_TOKEN= followed by 32 characters or more to that file (nuage genkey makes 64), then {}. Then come back here.",
        "{} n'a pas de jeton de gestion (NUAGE_ADMIN_TOKEN) : l'API de gestion du serveur est donc fermée. L'installateur n'en écrit un que lorsqu'il installe Nuage DB pour la première fois, pas lors d'une mise à jour : le relancer n'en ajoute aucun. Pour en ajouter un, en administrateur : ajoutez à ce fichier une ligne NUAGE_ADMIN_TOKEN= suivie d'au moins 32 caractères (nuage genkey en donne 64), puis {}. Revenez ensuite ici.",
        "{} no tiene token de gestión (NUAGE_ADMIN_TOKEN), así que la API de gestión del servidor está desactivada. El instalador solo escribe uno cuando instala Nuage DB por primera vez, no cuando lo actualiza: volver a ejecutarlo no añade ninguno. Para añadir uno, como administrador: añade a ese archivo una línea NUAGE_ADMIN_TOKEN= seguida de 32 caracteres o más (nuage genkey da 64) y después {}. Luego vuelve aquí.",
    ),
    (
        "Its settings file is for administrators only (it holds the encryption key). Windows asks for an administrator's consent: its window names Windows Command Processor, verified publisher Microsoft Windows, and its details show the command. That command runs findstr, which reads four lines of the file: PORT, NUAGE_ADMIN_TOKEN, SITE_URL and NUAGE_BIND. The encryption key is not read.",
        "Son fichier de réglages est réservé aux administrateurs (il contient la clé de chiffrement). Windows demande l'accord d'un administrateur : sa fenêtre nomme Processeur de commandes Windows, éditeur vérifié Microsoft Windows, et ses détails montrent la commande. Cette commande lance findstr, qui lit quatre lignes du fichier : PORT, NUAGE_ADMIN_TOKEN, SITE_URL et NUAGE_BIND. La clé de chiffrement n'est pas lue.",
        "Su archivo de ajustes es solo para administradores (contiene la clave de cifrado). Windows pide el consentimiento de un administrador: su ventana nombra Procesador de comandos de Windows, editor comprobado Microsoft Windows, y sus detalles muestran el comando. Ese comando ejecuta findstr, que lee cuatro líneas del archivo: PORT, NUAGE_ADMIN_TOKEN, SITE_URL y NUAGE_BIND. La clave de cifrado no se lee.",
    ),
    (
        "Its settings file is for administrators only (it holds the encryption key). macOS asks for an administrator's password, then runs grep as administrator, which reads four lines of the file: PORT, NUAGE_ADMIN_TOKEN, SITE_URL and NUAGE_BIND. The encryption key is not read.",
        "Son fichier de réglages est réservé aux administrateurs (il contient la clé de chiffrement). macOS demande le mot de passe d'un administrateur, puis lance grep en administrateur, qui lit quatre lignes du fichier : PORT, NUAGE_ADMIN_TOKEN, SITE_URL et NUAGE_BIND. La clé de chiffrement n'est pas lue.",
        "Su archivo de ajustes es solo para administradores (contiene la clave de cifrado). macOS pide la contraseña de un administrador y luego ejecuta grep como administrador, que lee cuatro líneas del archivo: PORT, NUAGE_ADMIN_TOKEN, SITE_URL y NUAGE_BIND. La clave de cifrado no se lee.",
    ),
    (
        "Its settings file is for administrators only (it holds the encryption key). Your system asks for an administrator's password, then runs grep as administrator, which reads four lines of the file: PORT, NUAGE_ADMIN_TOKEN, SITE_URL and NUAGE_BIND. The encryption key is not read.",
        "Son fichier de réglages est réservé aux administrateurs (il contient la clé de chiffrement). Votre système demande le mot de passe d'un administrateur, puis lance grep en administrateur, qui lit quatre lignes du fichier : PORT, NUAGE_ADMIN_TOKEN, SITE_URL et NUAGE_BIND. La clé de chiffrement n'est pas lue.",
        "Su archivo de ajustes es solo para administradores (contiene la clave de cifrado). Tu sistema pide la contraseña de un administrador y luego ejecuta grep como administrador, que lee cuatro líneas del archivo: PORT, NUAGE_ADMIN_TOKEN, SITE_URL y NUAGE_BIND. La clave de cifrado no se lee.",
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
