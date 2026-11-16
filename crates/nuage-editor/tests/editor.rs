//! The editor as a person uses it, against a real Nuage DB server running in
//! this process: the window is driven headless through its accessibility
//! tree (labels, clicks, typing), in French and in Spanish.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::{click, has, press, press_last, shows_anywhere, sized, type_into, wait_for, window, window_with};
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use nuage::{BackupOptions, DirectoryStore, Engine, EngineOptions, Server, ServerOptions, StartupSetting};
use nuage_editor::api::{self, ApiError, Transport};
use nuage_editor::clip::FakeSink;
use nuage_editor::i18n::Lang;
use nuage_editor::local::{Kind, Paths};
use nuage_editor::secret::Secret;
use nuage_editor::vault::{MemoryVault, Unavailable, Vault};
use nuage_editor::{LocalServer, Prefs, SavedServer, Setup};

const ADMIN: &str = "0123456789abcdef0123456789abcdef-editor";

/// A server in this process, with a management token and an application,
/// as the binary builds it (its environment says the cache budget), its
/// backups in a folder.
struct Running {
    _runtime: tokio::runtime::Runtime,
    _running: nuage::server::RunningServer,
    address: String,
    server: Arc<Server>,
    port: u16,
    dir: std::path::PathBuf,
}

fn start(name: &str) -> Running {
    let dir = std::env::temp_dir().join(format!("nuage-editor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let backup = BackupOptions::new(Arc::new(DirectoryStore::new(dir.join("backups"))));
    let options = EngineOptions { sync: false, backup: Some(backup), ..EngineOptions::default() };
    let engine = Engine::open(dir.join("data"), options).unwrap();
    let options = ServerOptions {
        admin_token: Some(ADMIN.into()),
        token: Some("first-token".into()),
        cache_budget: Some(48 << 20),
        cache_ceiling: Some(48 << 20),
        settings_env: Some(HashMap::from([("NUAGE_CACHE_MB".to_owned(), "48".to_owned())])),
        startup: vec![
            StartupSetting { var: "PORT", value: Some("8080".into()) },
            StartupSetting { var: "NUAGE_ADMIN_TOKEN", value: Some(StartupSetting::SECRET.into()) },
            StartupSetting { var: "NUAGE_TLS_CERT", value: None },
        ],
        ..ServerOptions::default()
    };
    let server = Arc::new(Server::new(engine, options));
    server.create_app("shop", "La boutique").unwrap();
    let running = runtime.block_on(server.listen("127.0.0.1:0")).unwrap();
    let port = running.local_addr().port();
    let address = format!("http://{}", running.local_addr());
    Running { _runtime: runtime, _running: running, address, server, port, dir }
}

impl Running {
    /// Every application copies itself to the backup folder.
    fn back_up(&self) {
        for app in self.server.apps() {
            self.server.app_engine(&app.name).unwrap().backup_now(true).unwrap();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn client(address: &str, token: &str, lang: Lang) -> api::Server {
    api::Server::new(address, Secret::new(token.into()), lang, &Transport::default()).unwrap()
}

fn in_lang(lang: Lang) -> Prefs {
    Prefs { lang: Some(lang), ..Prefs::default() }
}

fn french() -> Prefs {
    in_lang(Lang::Fr)
}

/// The window, connected to `running` with its management token (typed in the form).
fn connected(
    running: &Running,
    prefs: Prefs,
    clipboard: FakeSink,
) -> egui_kittest::Harness<'static, nuage_editor::Editor> {
    let l = prefs.lang.unwrap_or_default();
    let (address, token, connect) = match l {
        Lang::Fr => ("Adresse du serveur", "Jeton de gestion", "Se connecter"),
        Lang::Es => ("Dirección del servidor", "Token de gestión", "Conectar"),
        Lang::En => ("Server address", "Management token", "Connect"),
    };
    let mut harness = window(prefs, Arc::new(MemoryVault::default()), clipboard);
    type_into(&mut harness, address, &running.address);
    type_into(&mut harness, token, ADMIN);
    press(&mut harness, connect);
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    harness
}

/// The token a copy holds (`NUAGE_TOKEN=…`, or a page's script).
fn token_in(copied: &str) -> String {
    let (_, after) = copied.split_once("nuage_").unwrap();
    let rest: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    format!("nuage_{rest}")
}

#[test]
fn the_api_as_the_editor_calls_it() {
    let running = start("api");
    // The editor's rules are the server's.
    assert_eq!(api::MIN_ADMIN_TOKEN, nuage::server::MIN_ADMIN_TOKEN);
    let key = nuage_editor::app::genkey().unwrap();
    assert_eq!(nuage::Encryption::parse(key.expose()).unwrap().kind(), "key", "a token of Tools is a key too");
    let fr = client(&running.address, ADMIN, Lang::Fr);
    // Connecting: what this server has, the status included (the management token opens it).
    let connected = fr.clone().connect().unwrap();
    let caps = connected.server.caps();
    assert!(caps.settings && caps.status && caps.origins, "{caps:?}");
    assert_eq!(caps.version.as_deref(), Some(nuage::VERSION));
    let status = connected.status.unwrap();
    assert!(status.database("nuage").is_some() && status.database("shop").is_some(), "{status:?}");
    assert!(status.backup.enabled && status.backup.last_at.is_none(), "{:?}", status.backup);
    assert!(connected.settings.is_some() && connected.apps.iter().any(|a| a.name == "shop"));
    let settings = fr.settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().source, "environment");
    // A refusal comes back in the editor's language, with its figures.
    match fr.save_settings(&[("memory.cache_mb", Some("1".into()))]) {
        Err(ApiError::Invalid(m)) => assert!(m.contains("trop peu"), "{m}"),
        other => panic!("{other:?}"),
    }
    let saved = fr.save_settings(&[("memory.cache_mb", Some("32 Mio".into()))]).unwrap();
    assert_eq!(saved.get("memory.cache_mb").unwrap().value.as_deref(), Some("32"));
    let back = fr.reset_setting("memory.cache_mb").unwrap();
    assert_eq!(back.get("memory.cache_mb").unwrap().source, "environment");
    // Applications and their tokens; every name the editor accepts, the server does.
    let (app, connection) = fr.create_application("blog", "").unwrap();
    assert_eq!(app.name, "blog");
    assert!(connection.token.expose().starts_with("nuage_") && connection.env_lines().contains(&running.address));
    assert!(matches!(fr.create_application("blog", ""), Err(ApiError::Conflict(_))));
    for name in ["main", "Blog", "a b", ""] {
        assert!(!api::app_name_ok(name));
        assert!(matches!(fr.create_application(name, ""), Err(ApiError::Invalid(_))), "{name}");
    }
    // The tokens work, read and write or read only, and the server notes the editor's test.
    let ping = fr.ping_with(&connection.token).unwrap();
    assert_eq!((ping.application.as_str(), ping.read_only()), ("blog", false));
    let reader = fr.new_token("blog", true).unwrap();
    assert!(fr.ping_with(&reader.token).unwrap().read_only());
    let status = fr.status().unwrap();
    let last = status.database("blog").unwrap().last_connection.clone().unwrap();
    assert!(last.by_the_editor(), "{last:?}");
    let apps = fr.applications().unwrap();
    let blog = apps.iter().find(|a| a.name == "blog").unwrap();
    assert_eq!(blog.tokens.len(), 2);
    let id = blog.tokens.iter().find(|t| t.read_only).unwrap().id.clone();
    assert!(reader.token.expose().contains(&id));
    fr.revoke_token("blog", &id).unwrap();
    assert_eq!(fr.ping_with(&reader.token).unwrap_err(), ApiError::TokenRefused, "revoked");
    assert!(matches!(fr.revoke_token("blog", &id), Err(ApiError::NotFound(_))));
    // The websites of an application (S2): a browser's preflight from it is then allowed.
    let app = fr.set_origins("blog", &["https://monsite.fr".into(), "http://localhost:3000".into()]).unwrap();
    assert_eq!(app.origins, ["https://monsite.fr", "http://localhost:3000"]);
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let preflight = agent
        .options(&format!("{}/v1/sql", running.address))
        .header("origin", "https://monsite.fr")
        .header("access-control-request-method", "POST")
        .call()
        .unwrap();
    assert_eq!(preflight.headers().get("access-control-allow-origin").unwrap(), "https://monsite.fr");
    match fr.set_origins("blog", &["https://monsite.fr/page".into()]) {
        Err(ApiError::Invalid(m)) => assert!(m.contains("https://monsite.fr/page"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(fr.set_origins("blog", &[]).unwrap().origins.is_empty());
    assert!(matches!(fr.set_origins("nothing", &[]), Err(ApiError::NotFound(_))));
    let app = fr.set_memory("blog", Some("8 Mio"), Some("auto")).unwrap();
    assert_eq!((app.memory.cache_mb, app.memory.kv_mb), (Some(8), None));
    assert!(matches!(fr.set_memory("nothing", Some("8"), None), Err(ApiError::NotFound(_))));
    // A wrong token, an application's token, an address that is no server.
    let wrong = client(&running.address, "first-token", Lang::En).settings().unwrap_err();
    assert_eq!(wrong, ApiError::TokenRefused);
    let wrong = client(&running.address, "f".repeat(40).as_str(), Lang::En).connect().unwrap_err();
    assert_eq!(wrong, ApiError::TokenRefused);
    // An application's token opens its own status only, never the management API.
    let shop = client(&running.address, "first-token", Lang::En);
    assert_eq!(shop.applications().unwrap_err(), ApiError::TokenRefused);
    let site = client(&format!("{}/admin", running.address), ADMIN, Lang::En);
    assert_eq!(site.address(), running.address, "a page of the server is the server");
    // The lines given to an application use the server's public address when it has one.
    let public = client(&running.address, ADMIN, Lang::En).with_public(Some("https://nuage.example.com/".into()));
    let connection = public.new_token("blog", false).unwrap();
    assert_eq!(connection.url, "https://nuage.example.com");
    assert!(!connection.is_this_computer_only());
}

#[test]
fn a_person_connects_and_sees_that_it_works_in_french() {
    let running = start("window");
    let mut harness = window(french(), Arc::new(MemoryVault::default()), FakeSink::default());
    assert!(has(&harness, "Bienvenue") && has(&harness, "Mon serveur est ailleurs"));

    // Nothing typed: the window says what is missing.
    click(&mut harness, "Se connecter");
    assert!(has(&harness, "Tapez d'abord l'adresse du serveur"));

    // The address the editor will call shows before anything is sent.
    type_into(&mut harness, "Adresse du serveur", &format!("{}/admin/settings", running.address));
    assert!(has(&harness, &format!("Connexion à {}", running.address)) && has(&harness, "(/admin/settings retiré)"));

    // Where the token is, by host: Heroku's dashboard.
    harness.get_by_label("Où le trouver ?").click();
    harness.run();
    assert!(has(&harness, "Reveal Config Vars") && has(&harness, "Ouvrir le tableau de bord Heroku"));

    // A token too short, an application's token: stopped before any request.
    type_into(&mut harness, "Jeton de gestion", "first-token");
    click(&mut harness, "Se connecter");
    assert!(has(&harness, "Un jeton de gestion a au moins 32 caractères"));
    type_into(&mut harness, "Jeton de gestion", "nuage_0123456789abcdef0123456789abcdef");
    click(&mut harness, "Se connecter");
    assert!(has(&harness, "Ceci est le jeton d'une application"));

    // A wrong token: refused at the second step, and the window says which token it needs.
    type_into(&mut harness, "Jeton de gestion", &"f".repeat(40));
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the refusal", |h| has(h, "Le serveur a refusé ce jeton"));
    assert!(has(&harness, "Recherche du serveur") && has(&harness, "Vérification du jeton"));
    assert!(
        harness.query_by_label("Réessayer").is_some()
            && harness.query_by_label("Changer l'adresse ou le jeton").is_some()
    );

    // The right one, pasted with its name.
    type_into(&mut harness, "Jeton de gestion", &format!("NUAGE_ADMIN_TOKEN={ADMIN}"));
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert_eq!(harness.state().connected_to(), Some(running.address.as_str()));
    assert_eq!(harness.state().prefs.servers, vec![SavedServer::at(&running.address)], "remembered (the address only)");
    assert!(!shows_anywhere(&harness, ADMIN), "the token is in no field any more");

    // The overview: it works, its version, its applications, its memory, its backups.
    assert!(has(&harness, "Nuage DB répond") && has(&harness, &format!("version {}", nuage::VERSION)));
    assert!(has(&harness, "en marche depuis"));
    assert!(has(&harness, "2 applications"));
    assert!(has(&harness, "Nuage DB occupe"), "the memory, from the status");
    assert!(has(&harness, "Non chiffré : cet ordinateur seulement"));
    // The engine may have made its first copy already.
    assert!(has(&harness, "En attente de la première copie, vers") || has(&harness, "Dernière copie il y a"));
    running.back_up();
    press(&mut harness, "Actualiser");
    wait_for(&mut harness, "the backups", |h| has(h, "Dernière copie il y a"));
    assert!(has(&harness, &running.dir.join("backups").display().to_string()));
    assert!(has(&harness, "Mis à jour il y a"));
}

#[test]
fn memory_and_limits_are_in_force_at_once() {
    let running = start("memory");
    let mut harness = connected(&running, french(), FakeSink::default());
    click(&mut harness, "Mémoire");
    assert!(has(&harness, "Vient de l'environnement (NUAGE_CACHE_MB=48)"));
    type_into(&mut harness, "Caches de pages, toutes les applications ensemble", "32 Mio");
    press(&mut harness, "Enregistrer la mémoire");
    wait_for(&mut harness, "the save", |h| has(h, "en vigueur tout de suite"));
    let settings = client(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().value.as_deref(), Some("32"));
    assert_eq!(settings.memory.cache_budget_bytes, Some(32 << 20));
    assert!(has(&harness, "Enregistré ici : il l'emporte sur l'environnement"));

    // Given back to the environment, with one click.
    press(&mut harness, "Revenir à la valeur de l'environnement");
    wait_for(&mut harness, "the reset", |h| has(h, "Vient de l'environnement (NUAGE_CACHE_MB=48)"));
    let settings = client(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().source, "environment");

    // Too much for one application: refused in French, with the figures.
    type_into(&mut harness, "shop · Cache de pages", "2 Go");
    press(&mut harness, "Enregistrer · shop");
    wait_for(&mut harness, "the refusal", |h| has(h, "c'est plus que ce qui reste"));

    // The limits: out of range, said before anything is sent.
    click(&mut harness, "Limites et visibilité");
    type_into(&mut harness, "Connexions PostgreSQL simultanées", "0");
    click(&mut harness, "Enregistrer les limites");
    assert!(has(&harness, "n'est pas entre 1 et 10 000"));
    type_into(&mut harness, "Connexions PostgreSQL simultanées", "12");
    press(&mut harness, "Enregistrer les limites");
    wait_for(&mut harness, "the save", |h| has(h, "en vigueur tout de suite"));
    let settings = client(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("postgres.connections").unwrap().value.as_deref(), Some("12"));

    // What it was started with: a secret is never shown.
    click(&mut harness, "Réglés au démarrage");
    assert!(has(&harness, "NUAGE_ADMIN_TOKEN") && has(&harness, "réglé (un secret : non affiché)"));
    assert!(!shows_anywhere(&harness, ADMIN));
}

#[test]
fn a_refusal_comes_back_in_spanish() {
    let running = start("spanish");
    let mut harness = connected(&running, in_lang(Lang::Es), FakeSink::default());
    assert!(has(&harness, "Nuage DB responde") && has(&harness, "Copias de seguridad"));
    click(&mut harness, "Memoria");
    type_into(&mut harness, "Cachés de páginas, todas las aplicaciones juntas", "1");
    press(&mut harness, "Guardar la memoria");
    wait_for(&mut harness, "the refusal", |h| has(h, "es demasiado poco"));
    assert!(has(&harness, "No guardado:"));
    let settings = client(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().source, "environment", "nothing saved");
    type_into(&mut harness, "shop · Caché de páginas", "2 GiB");
    press(&mut harness, "Guardar · shop");
    wait_for(&mut harness, "the refusal", |h| has(h, "es más de lo que queda"));
}

#[test]
fn a_person_makes_an_application_and_tests_its_token() {
    let running = start("apps");
    let clipboard = FakeSink::default();
    let mut harness = connected(&running, french(), clipboard.clone());
    click(&mut harness, "Applications");
    assert!(has(&harness, "La boutique") && has(&harness, "principale"));

    // What it is for: its name follows, until the name is typed.
    type_into(&mut harness, "À quoi sert-elle ?", "Ma boutique");
    assert!(has(&harness, "Son nom sera : ma-boutique"));
    press(&mut harness, "Créer l'application");
    wait_for(&mut harness, "the token card", |h| has(h, "Copiez ce jeton maintenant"));
    assert!(has(&harness, "Application ma-boutique créée"));
    assert!(running.server.apps().iter().any(|a| a.name == "ma-boutique" && a.description == "Ma boutique"));
    // Masked: the token is in no node until it is shown.
    let made = running.server.apps().into_iter().find(|a| a.name == "ma-boutique").unwrap();
    let id = made.tokens[0].id.clone();
    assert!(!shows_anywhere(&harness, &id), "masked");
    // A server reached on 127.0.0.1 without a public address: the card says where it works.
    assert!(has(&harness, "Cette adresse fonctionne pour un programme de cet ordinateur"));

    // Escape and closing without a copy keep it.
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(has(&harness, "Copiez ce jeton maintenant"), "Escape does not close it");
    click(&mut harness, "Je l'ai copié");
    assert!(has(&harness, "Fermer sans avoir copié le jeton ?"));
    click(&mut harness, "Revenir");
    assert!(has(&harness, "Copiez ce jeton maintenant"));

    // Copied the private way, then tested from this computer.
    click(&mut harness, "Copier les deux lignes");
    let copies = clipboard.copies();
    assert_eq!(copies.len(), 1);
    assert!(copies[0].starts_with(&format!("NUAGE_URL={}\nNUAGE_TOKEN=nuage_", running.address)), "{}", copies[0]);
    let token = token_in(&copies[0]);
    assert!(token.contains(&id));
    assert!(has(&harness, "l'historique du presse-papiers est prié de l'ignorer"));
    press(&mut harness, "Tester le jeton");
    wait_for(&mut harness, "the test", |h| has(h, "Il fonctionne : il ouvre ma-boutique, en lecture et écriture"));
    click(&mut harness, "Vider le presse-papiers");
    assert_eq!(clipboard.copies().last().map(String::as_str), Some(""), "the clipboard emptied");
    click(&mut harness, "Je l'ai copié");
    assert!(!has(&harness, "Copiez ce jeton maintenant"), "shown once");
    assert!(!shows_anywhere(&harness, &token), "in no node once closed");
    assert!(!format!("{:?}", harness.state()).contains(&token));
    // Its row says the editor tested it, and that the site has not connected yet.
    wait_for(&mut harness, "the test in its row", |h| has(h, "Testée depuis l'éditeur il y a"));
    assert!(has(&harness, "votre site ne s'est pas encore connecté"));

    // The same name again: refused, under the field.
    type_into(&mut harness, "À quoi sert-elle ?", "Ma boutique");
    press(&mut harness, "Créer l'application");
    wait_for(&mut harness, "the refusal", |h| has(h, "Il y a déjà une application « ma-boutique »"));
}

#[test]
fn a_web_page_gets_a_read_only_token_and_its_website() {
    let running = start("website");
    let clipboard = FakeSink::default();
    let mut harness = connected(&running, french(), clipboard.clone());
    click(&mut harness, "Applications");
    click(&mut harness, "shop");
    assert!(has(&harness, "Sites web autorisés") && has(&harness, "Aucun pour l'instant"));
    assert!(has(&harness, "NUAGE_TOKEN=<un jeton de cette application>"));

    // A read-only token: the card says no website may call it yet, and what the page holds.
    press(&mut harness, "Nouveau jeton en lecture seule, pour une page web");
    wait_for(&mut harness, "the token card", |h| has(h, "Nouveau jeton en lecture seule pour shop"));
    assert!(has(&harness, "Aucun site ne peut encore appeler cette application"));
    assert!(has(&harness, "Collez ceci dans le script de la page"));
    click(&mut harness, "Copier pour la page web");
    let page = clipboard.copies().pop().unwrap();
    assert!(
        page.contains(&format!("const NUAGE_URL = \"{}\";", running.address)) && page.contains("/v1/sql"),
        "{page}"
    );
    let token = token_in(&page);
    let shop = client(&running.address, ADMIN, Lang::En);
    assert!(shop.ping_with(&Secret::new(token.clone())).unwrap().read_only());
    click(&mut harness, "Aller aux sites web autorisés");
    assert!(!has(&harness, "Nouveau jeton en lecture seule pour shop"), "copied: closed");

    // Its website, written as people write it: what the server keeps.
    type_into(&mut harness, "Site à autoriser", "monsite.fr/page");
    click(&mut harness, "Ajouter");
    assert!(has(&harness, "Ce n'est pas le début de l'adresse d'un site"), "said before sending");
    type_into(&mut harness, "Site à autoriser", "HTTPS://MonSite.fr/");
    press(&mut harness, "Ajouter");
    wait_for(&mut harness, "the website", |h| {
        has(h, "Enregistré : les pages de https://monsite.fr peuvent appeler shop")
    });
    let origins = running.server.apps().into_iter().find(|a| a.name == "shop").unwrap().origins;
    assert_eq!(origins, ["https://monsite.fr"]);
    type_into(&mut harness, "Site à autoriser", "https://monsite.fr");
    click(&mut harness, "Ajouter");
    assert!(has(&harness, "https://monsite.fr est déjà autorisé."));

    // Revoked after asking: Escape keeps the token.
    let revokes = harness.query_all_by_role_and_label(Role::Button, "Révoquer").count();
    assert_eq!(revokes, 2, "shop's first token, and the read-only one");
    harness.query_all_by_role_and_label(Role::Button, "Révoquer").last().unwrap().click();
    harness.run();
    assert!(has(&harness, "Révoquer ce jeton ?"));
    harness.key_press(egui::Key::Escape);
    harness.run();
    assert!(!has(&harness, "Révoquer ce jeton ?"), "Escape keeps it");
    assert!(shop.ping_with(&Secret::new(token.clone())).is_ok());
    harness.query_all_by_role_and_label(Role::Button, "Révoquer").last().unwrap().click();
    harness.run();
    press_last(&mut harness, "Révoquer");
    wait_for(&mut harness, "the revocation", |h| has(h, "révoqué : il n'ouvre plus rien"));
    assert_eq!(shop.ping_with(&Secret::new(token)).unwrap_err(), ApiError::TokenRefused);

    // The website removed: none may call it any more.
    press(&mut harness, "Retirer");
    wait_for(&mut harness, "the removal", |h| has(h, "Plus aucun site ne peut appeler shop"));
    assert!(running.server.apps().into_iter().find(|a| a.name == "shop").unwrap().origins.is_empty());
}

#[test]
fn this_computers_server_opens_with_nothing_to_type() {
    let running = start("here");
    // The layout the Linux installer writes, readable by the person (as on macOS for the account that installed).
    let root = running.dir.join("root");
    let paths = Paths::under(&root, Kind::Linux);
    std::fs::create_dir_all(paths.settings.parent().unwrap()).unwrap();
    std::fs::create_dir_all(paths.service.parent().unwrap()).unwrap();
    std::fs::write(&paths.service, "[Unit]\n").unwrap();
    let file = format!(
        "NUAGE_ENCRYPTION_KEY=never-read\nPORT={}\nNUAGE_ADMIN_TOKEN={ADMIN}\nNUAGE_BIND=127.0.0.1\n",
        running.port
    );
    std::fs::write(&paths.settings, file).unwrap();
    let mut setup = Setup::with(Arc::new(MemoryVault::default()), Box::new(FakeSink::default()));
    setup.local = LocalServer::At(paths);
    let mut harness = window_with(french(), setup);
    assert!(has(&harness, "Nuage DB est installé sur cet ordinateur.") && has(&harness, "Rien à taper."));
    assert!(has(&harness, &format!("Répond sur le port {}", running.port)));
    press(&mut harness, "Ouvrir");
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert_eq!(harness.state().connected_to(), Some(format!("http://127.0.0.1:{}", running.port).as_str()));
    // Read from the file, which can be read again: nothing kept.
    let saved = &harness.state().prefs.servers[0];
    assert!(saved.local && !saved.token_kept, "{saved:?}");
    assert!(!shows_anywhere(&harness, ADMIN));
    assert!(has(&harness, "Cet ordinateur"), "its page, in the sections");
}

#[test]
fn a_kept_token_is_used_and_never_shown() {
    let running = start("kept");
    let vault = Arc::new(MemoryVault::default());
    let mut harness = window(french(), vault.clone(), FakeSink::default());
    type_into(&mut harness, "Adresse du serveur", &running.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Retenir sur cet ordinateur").click();
    harness.run();
    assert!(has(&harness, "Le jeton va dans la mémoire de l'éditeur"));
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert_eq!(vault.addresses(), vec![running.address.clone()], "kept in the store");
    assert_eq!(vault.get(&running.address).unwrap().unwrap().expose(), ADMIN);
    assert!(harness.state().prefs.servers[0].token_kept);
    // What eframe writes, and what a bug report would print, hold no token.
    let prefs = harness.state().prefs.clone();
    let written = serde_json::to_string(&prefs).unwrap();
    assert!(!written.contains(ADMIN) && !written.contains("nuage_"), "{written}");
    assert!(!format!("{:?}", harness.state()).contains(ADMIN));
    assert!(!shows_anywhere(&harness, ADMIN));
    drop(harness);

    // The next launch: the token is used from the store, never put in a field.
    let mut harness = window(prefs.clone(), vault.clone(), FakeSink::default());
    assert!(has(&harness, "Jeton gardé dans la mémoire de l'éditeur"));
    assert!(harness.query_by_label("Afficher").is_none(), "no Show for a kept token");
    assert_eq!(harness.query_all_by_role(Role::PasswordInput).count(), 0, "no token field");
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert!(!shows_anywhere(&harness, ADMIN));
    click(&mut harness, "Se déconnecter");

    // Forgotten: out of the store, and the field is back.
    press(&mut harness, "Oublier ce jeton");
    wait_for(&mut harness, "forgotten", |h| has(h, "Jeton oublié"));
    assert!(vault.addresses().is_empty());
    assert!(!harness.state().prefs.servers[0].token_kept);
    assert_eq!(harness.query_all_by_role(Role::PasswordInput).count(), 1);
    drop(harness);

    // A kept token changed on the server since: said, and kept until a new one works.
    vault.set(&running.address, &Secret::new("e".repeat(40))).unwrap();
    let kept =
        Prefs { servers: vec![SavedServer { token_kept: true, ..SavedServer::at(&running.address) }], ..french() };
    let mut harness = window(kept, vault.clone(), FakeSink::default());
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the refusal", |h| has(h, "Le jeton gardé ne fonctionne plus"));
    assert_eq!(vault.addresses(), vec![running.address.clone()], "not deleted without a word");
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert_eq!(vault.get(&running.address).unwrap().unwrap().expose(), ADMIN, "replaced once the new one works");
}

#[test]
fn without_a_password_store_nothing_is_kept() {
    let running = start("nostore");
    let vault = Arc::new(Unavailable("no Secret Service on this session".into()));
    let mut harness = window(french(), vault, FakeSink::default());
    assert!(has(&harness, "Votre système n'a pas de gestionnaire de mots de passe que l'éditeur puisse utiliser"));
    assert!(has(&harness, "no Secret Service on this session"));
    assert!(harness.get_by_label("Retenir sur cet ordinateur").accesskit_node().is_disabled());
    type_into(&mut harness, "Adresse du serveur", &running.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert!(!harness.state().prefs.servers[0].token_kept);
}

#[test]
fn a_token_the_store_no_longer_has() {
    // Removed from the store by hand (the system's password manager): said, and the field comes back.
    let kept =
        Prefs { servers: vec![SavedServer { token_kept: true, ..SavedServer::at("http://127.0.0.1:9") }], ..french() };
    let mut harness = window(kept, Arc::new(MemoryVault::default()), FakeSink::default());
    assert!(has(&harness, "Jeton gardé dans"));
    press(&mut harness, "Se connecter");
    wait_for(&mut harness, "the notice", |h| has(h, "Aucun jeton n'est plus gardé pour ce serveur"));
    assert!(!harness.state().prefs.servers[0].token_kept);
    assert_eq!(harness.query_all_by_role(Role::PasswordInput).count(), 1);
}

#[test]
fn the_pages_fit_the_smallest_window_and_every_language() {
    // 640×480, the window's smallest, in the longest languages and both themes: every
    // page draws (a missing glyph or translation would stop it), the token card included.
    let running = start("small");
    for (lang, theme) in [(Lang::Fr, nuage_editor::app::Theme::Dark), (Lang::Es, nuage_editor::app::Theme::Light)] {
        let prefs = Prefs { lang: Some(lang), theme, ..Prefs::default() };
        let setup = Setup::with(Arc::new(MemoryVault::default()), Box::new(FakeSink::default()));
        let mut harness = sized(prefs, setup, [640.0, 480.0]);
        let (address, token, connect) = match lang {
            Lang::Fr => ("Adresse du serveur", "Jeton de gestion", "Se connecter"),
            _ => ("Dirección del servidor", "Token de gestión", "Conectar"),
        };
        type_into(&mut harness, address, &running.address);
        type_into(&mut harness, token, ADMIN);
        press(&mut harness, connect);
        wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
        let sections: &[&str] = match lang {
            Lang::Fr => &[
                "Applications",
                "Mémoire",
                "Limites et visibilité",
                "Réglés au démarrage",
                "Outils",
                "À propos",
                "Vue d'ensemble",
            ],
            _ => &[
                "Aplicaciones",
                "Memoria",
                "Límites y visibilidad",
                "Definidos al arrancar",
                "Herramientas",
                "Acerca de",
                "Resumen",
            ],
        };
        for section in sections {
            click(&mut harness, section);
            assert!(harness.state().failure().is_none(), "{lang:?} {section}: {:?}", harness.state().failure());
        }
        // The token card: its buttons stay reachable in the smallest window.
        click(&mut harness, sections[0]);
        click(&mut harness, "shop");
        let new =
            if lang == Lang::Fr { "Nouveau jeton (lecture et écriture)" } else { "Nuevo token (lectura y escritura)" };
        press(&mut harness, new);
        let copy = if lang == Lang::Fr { "Copier les deux lignes" } else { "Copiar las dos líneas" };
        wait_for(&mut harness, "the token card", |h| has(h, copy));
        let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0));
        let close = if lang == Lang::Fr { "Je l'ai copié" } else { "Lo he copiado" };
        harness.get_by_role_and_label(Role::Button, copy).click();
        harness.run();
        let button = harness.get_by_role_and_label(Role::Button, close);
        button.scroll_to_me();
        harness.run();
        let bounds = harness.get_by_role_and_label(Role::Button, close).rect();
        assert!(window.contains_rect(bounds), "{lang:?}: {bounds:?} is outside the window");
        harness.get_by_role_and_label(Role::Button, close).click();
        harness.run();
        assert!(harness.state().failure().is_none());
    }
}

#[test]
fn a_bug_in_the_window_shows_a_page_and_the_way_back() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // One frame of the window panics (a made-up bug): the window stays, and says so.
    let fail_once = Arc::new(AtomicBool::new(true));
    let fail = fail_once.clone();
    let editor = nuage_editor::Editor::new(
        french(),
        nuage_editor::Setup::with(Arc::new(MemoryVault::default()), Box::new(FakeSink::default())),
    );
    let mut harness = egui_kittest::Harness::builder().with_size([1100.0, 900.0]).with_max_steps(30).build_ui_state(
        move |ui, editor: &mut nuage_editor::Editor| {
            editor.frame_with(ui, |editor, ui| {
                editor.show(ui);
                if fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    panic!("made-up bug in the window");
                }
            })
        },
        editor,
    );
    harness.run();
    assert!(!fail_once.load(Ordering::SeqCst));
    assert_eq!(harness.state().failure().map(|f| f.message.as_str()), Some("made-up bug in the window"));
    assert!(has(&harness, "L'éditeur a rencontré un bug"));
    assert!(shows_anywhere(&harness, "made-up bug in the window"));
    click(&mut harness, "Revenir au début");
    assert!(harness.state().failure().is_none());
    wait_for(&mut harness, "the start page", |h| has(h, "Mon serveur est ailleurs"));
}
