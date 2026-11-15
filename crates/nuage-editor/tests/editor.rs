//! The editor as a person uses it, against a real Nuage DB server running in
//! this process: the window is driven headless through its accessibility
//! tree (labels, clicks, typing), in French.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::{click, has, shows_anywhere, type_into, wait_for, window};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT as _, Queryable as _};
use nuage::{Engine, EngineOptions, Server, ServerOptions};
use nuage_editor::api::{self, ApiError, Transport};
use nuage_editor::clip::FakeSink;
use nuage_editor::i18n::Lang;
use nuage_editor::secret::Secret;
use nuage_editor::vault::{MemoryVault, Unavailable, Vault};
use nuage_editor::{Prefs, SavedServer};

const ADMIN: &str = "0123456789abcdef0123456789abcdef-editor";

/// A server in this process, with a management token and an application,
/// as the binary builds it (its environment says the cache budget).
struct Running {
    _runtime: tokio::runtime::Runtime,
    _running: nuage::server::RunningServer,
    address: String,
    server: Arc<Server>,
}

fn start(name: &str) -> Running {
    let dir = std::env::temp_dir().join(format!("nuage-editor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let engine = Engine::open(&dir, EngineOptions { sync: false, ..EngineOptions::default() }).unwrap();
    let options = ServerOptions {
        admin_token: Some(ADMIN.into()),
        token: Some("first-token".into()),
        cache_budget: Some(48 << 20),
        cache_ceiling: Some(48 << 20),
        settings_env: Some(HashMap::from([("NUAGE_CACHE_MB".to_owned(), "48".to_owned())])),
        ..ServerOptions::default()
    };
    let server = Arc::new(Server::new(engine, options));
    server.create_app("shop", "La boutique").unwrap();
    let running = runtime.block_on(server.listen("127.0.0.1:0")).unwrap();
    let address = format!("http://{}", running.local_addr());
    Running { _runtime: runtime, _running: running, address, server }
}

fn client(address: &str, token: &str, lang: Lang) -> api::Server {
    api::Server::new(address, Secret::new(token.into()), lang, &Transport::default()).unwrap()
}

fn french() -> Prefs {
    Prefs { lang: Some(Lang::Fr), ..Prefs::default() }
}

#[test]
fn the_api_as_the_editor_calls_it() {
    let running = start("api");
    // The editor's rule is the server's.
    assert_eq!(api::MIN_ADMIN_TOKEN, nuage::server::MIN_ADMIN_TOKEN);
    let fr = client(&running.address, ADMIN, Lang::Fr);
    // Connecting: what this server has.
    let connected = fr.clone().connect().unwrap();
    let caps = connected.server.caps();
    assert!(caps.settings, "{caps:?}");
    assert!(connected.settings.is_some() && connected.apps.iter().any(|a| a.name == "shop"));
    let settings = fr.settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().source, "environment");
    assert!(settings.applications.iter().any(|a| a.name == "shop"));
    // A refusal comes back in the editor's language, with its figures.
    match fr.save_settings(&[("memory.cache_mb", Some("1".into()))]) {
        Err(ApiError::Invalid(m)) => assert!(m.contains("trop peu"), "{m}"),
        other => panic!("{other:?}"),
    }
    let saved = fr.save_settings(&[("memory.cache_mb", Some("32 Mio".into()))]).unwrap();
    assert_eq!(saved.get("memory.cache_mb").unwrap().value.as_deref(), Some("32"));
    let back = fr.reset_setting("memory.cache_mb").unwrap();
    assert_eq!(back.get("memory.cache_mb").unwrap().source, "environment");
    // Applications and their tokens.
    let (app, connection) = fr.create_application("blog", "").unwrap();
    assert_eq!(app.name, "blog");
    assert!(connection.token.expose().starts_with("nuage_") && connection.env_lines().contains(&running.address));
    assert!(matches!(fr.create_application("blog", ""), Err(ApiError::Conflict(_))));
    let reader = fr.new_token("blog", true).unwrap();
    let apps = fr.applications().unwrap();
    let blog = apps.iter().find(|a| a.name == "blog").unwrap();
    assert_eq!(blog.tokens.len(), 2);
    let id = blog.tokens.iter().find(|t| t.read_only).unwrap().id.clone();
    assert!(reader.token.expose().contains(&id));
    fr.revoke_token("blog", &id).unwrap();
    let app = fr.set_memory("blog", Some("8 Mio"), Some("auto")).unwrap();
    assert_eq!((app.memory.cache_mb, app.memory.kv_mb), (Some(8), None));
    assert!(matches!(fr.set_memory("nothing", Some("8"), None), Err(ApiError::NotFound(_))));
    // A wrong token, an application's token, an address that is no server.
    let wrong = client(&running.address, "first-token", Lang::En).settings().unwrap_err();
    assert_eq!(wrong, ApiError::TokenRefused);
    let wrong = client(&running.address, "f".repeat(40).as_str(), Lang::En).connect().unwrap_err();
    assert_eq!(wrong, ApiError::TokenRefused);
    let site = client(&format!("{}/admin", running.address), ADMIN, Lang::En);
    assert_eq!(site.address(), running.address, "a page of the server is the server");
    assert!(running.server.apps().iter().any(|a| a.name == "blog"));
    // The lines given to an application use the server's public address when it has one.
    let public = client(&running.address, ADMIN, Lang::En).with_public(Some("https://nuage.example.com/".into()));
    let connection = public.new_token("blog", false).unwrap();
    assert_eq!(connection.url, "https://nuage.example.com");
    assert!(!connection.is_this_computer_only());
}

#[test]
fn a_person_connects_and_changes_the_memory_in_french() {
    let running = start("window");
    let mut harness = window(french(), Arc::new(MemoryVault::default()), FakeSink::default());
    assert!(has(&harness, "Se connecter à votre serveur Nuage DB"));

    // Nothing typed: the window says what is missing.
    harness.get_by_label("Se connecter").click();
    harness.run();
    assert!(has(&harness, "Tapez d'abord l'adresse du serveur"));

    // The address the editor will call shows before anything is sent.
    type_into(&mut harness, "Adresse du serveur", &format!("{}/admin/settings", running.address));
    assert!(has(&harness, &format!("Connexion à {}", running.address)) && has(&harness, "(/admin/settings retiré)"));

    // A token too short, an application's token: stopped before any request.
    type_into(&mut harness, "Jeton de gestion", "first-token");
    harness.get_by_label("Se connecter").click();
    harness.run();
    assert!(has(&harness, "Un jeton de gestion a au moins 32 caractères"));
    type_into(&mut harness, "Jeton de gestion", "nuage_0123456789abcdef0123456789abcdef");
    harness.get_by_label("Se connecter").click();
    harness.run();
    assert!(has(&harness, "Ceci est le jeton d'une application"));

    // A wrong token: refused, and the window says which token it needs.
    type_into(&mut harness, "Jeton de gestion", &"f".repeat(40));
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the refusal", |h| has(h, "Le serveur a refusé ce jeton"));

    // The right one, pasted with its name.
    type_into(&mut harness, "Jeton de gestion", &format!("NUAGE_ADMIN_TOKEN={ADMIN}"));
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert_eq!(harness.state().connected_to(), Some(running.address.as_str()));
    assert_eq!(
        harness.state().prefs.servers,
        vec![SavedServer { address: running.address.clone(), token_kept: false }],
        "remembered (the address only)"
    );
    wait_for(&mut harness, "the overview", |h| has(h, "répond, et son jeton de gestion est le bon"));
    assert!(!shows_anywhere(&harness, ADMIN), "the token is in no field any more");

    // Memory: the cache budget of the server, written the French way.
    click(&mut harness, "Mémoire");
    assert!(has(&harness, "Vient de l'environnement (NUAGE_CACHE_MB=48)"));
    type_into(&mut harness, "Caches de pages, toutes les applications ensemble", "32 Mio");
    harness.get_by_label("Enregistrer la mémoire").click();
    wait_for(&mut harness, "the save", |h| has(h, "en vigueur tout de suite"));
    let settings = client(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().value.as_deref(), Some("32"));
    assert_eq!(settings.memory.cache_budget_bytes, Some(32 << 20));
    wait_for(&mut harness, "the source line", |h| has(h, "Enregistré ici : il l'emporte sur l'environnement"));

    // Given back to the environment, with one click.
    harness.get_by_label("Revenir à la valeur de l'environnement").click();
    wait_for(&mut harness, "the reset", |h| has(h, "Vient de l'environnement (NUAGE_CACHE_MB=48)"));
    let settings = client(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().source, "environment");

    // Too much for one application: refused in French, with the figures.
    type_into(&mut harness, "shop · Cache de pages", "2 Go");
    harness.get_by_label("Enregistrer · shop").click();
    wait_for(&mut harness, "the refusal", |h| has(h, "c'est plus que ce qui reste"));
}

#[test]
fn a_person_makes_an_application_and_copies_its_token() {
    let running = start("apps");
    let clipboard = FakeSink::default();
    let mut harness = window(french(), Arc::new(MemoryVault::default()), clipboard.clone());
    type_into(&mut harness, "Adresse du serveur", &running.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());

    click(&mut harness, "Applications");
    wait_for(&mut harness, "the applications", |h| has(h, "La boutique"));
    type_into(&mut harness, "Nom", "blog");
    harness.get_by_label("Créer l'application").click();
    wait_for(&mut harness, "the token window", |h| has(h, "Copiez-le maintenant"));
    let lines = harness.get_by_label("Lignes de connexion").value().unwrap_or_default();
    assert!(lines.contains("NUAGE_TOKEN=nuage_") && lines.contains(&running.address), "{lines}");
    // A server reached on 127.0.0.1 without a public address: the window says where it works.
    assert!(has(&harness, "Cette adresse ne fonctionne que pour un programme de cet ordinateur"));
    // The token opens the new application, and only it.
    let token = lines.lines().find_map(|l| l.strip_prefix("NUAGE_TOKEN=")).unwrap().to_owned();
    assert!(running.server.apps().iter().any(|a| a.name == "blog"));
    // Copied the private way (the clipboard history is asked to skip it).
    click(&mut harness, "Copier les deux lignes");
    click(&mut harness, "Copier le jeton");
    assert_eq!(clipboard.copies(), vec![lines.clone(), token.clone()]);
    assert!(
        has(&harness, "l'historique du presse-papiers est prié de l'ignorer") || has(&harness, "priés de l'ignorer")
    );
    harness.get_by_label("Je l'ai copié : fermer").click();
    harness.run();
    assert!(!has(&harness, "Copiez-le maintenant"), "shown once");
    assert!(!shows_anywhere(&harness, &token), "in no field once closed");
    assert!(!format!("{:?}", harness.state()).contains(&token));

    // A read-only token for the website, then revoked.
    // The applications in their order: the server's own, then by name (blog, shop).
    wait_for(&mut harness, "the new application", |h| has(h, "blog") && has(h, "La boutique"));
    let buttons: Vec<_> = harness.query_all_by_label("Nouveau jeton en lecture seule").collect();
    assert_eq!(buttons.len(), 3, "one for each application");
    buttons[1].click();
    harness.run();
    wait_for(&mut harness, "the second token window", |h| has(h, "Lecture seule : pour les pages d'un site web"));
    harness.get_by_label("Je l'ai copié : fermer").click();
    harness.run();
    let blog = running.server.apps().into_iter().find(|a| a.name == "blog").unwrap();
    assert_eq!(blog.tokens.iter().filter(|t| t.read_only).count(), 1, "the read-only token was made");
    wait_for(&mut harness, "the revoke button", |h| h.query_all_by_label("Révoquer").count() >= 2);
    let revoke: Vec<_> = harness.query_all_by_label("Révoquer").collect();
    revoke[0].click();
    harness.run();
    assert!(has(&harness, "Révoquer ce jeton ?"));
    harness.get_by_label("Annuler").click();
    harness.run();
    assert!(!has(&harness, "Révoquer ce jeton ?"), "cancelled");
    let tokens: usize = running.server.apps().iter().map(|a| a.tokens.len()).sum();
    assert_eq!(tokens, 3, "nothing revoked: shop's, and blog's two");
}

#[test]
fn a_kept_token_is_used_and_never_shown() {
    let running = start("kept");
    let vault = Arc::new(MemoryVault::default());
    let mut harness = window(french(), vault.clone(), FakeSink::default());
    wait_for(&mut harness, "the password store", |h| h.state().can_keep_tokens().is_some());
    type_into(&mut harness, "Adresse du serveur", &running.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Retenir sur cet ordinateur").click();
    harness.run();
    assert!(has(&harness, "Le jeton va dans la mémoire de l'éditeur"));
    harness.get_by_label("Se connecter").click();
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
    wait_for(&mut harness, "the password store", |h| h.state().can_keep_tokens().is_some());
    assert!(has(&harness, "Jeton gardé dans la mémoire de l'éditeur"));
    assert!(harness.query_by_label("Afficher").is_none(), "no Show for a kept token");
    assert_eq!(harness.query_all_by_role(Role::PasswordInput).count(), 0, "no token field");
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert!(!shows_anywhere(&harness, ADMIN));
    click(&mut harness, "Se déconnecter");

    // Forgotten: out of the store, and the field is back.
    click(&mut harness, "Oublier ce jeton");
    wait_for(&mut harness, "forgotten", |h| has(h, "Jeton oublié"));
    assert!(vault.addresses().is_empty());
    assert!(!harness.state().prefs.servers[0].token_kept);
    assert_eq!(harness.query_all_by_role(Role::PasswordInput).count(), 1);
    drop(harness);

    // A kept token changed on the server since: said, and kept until a new one works.
    vault.set(&running.address, &Secret::new("e".repeat(40))).unwrap();
    let kept = Prefs { servers: vec![SavedServer { address: running.address.clone(), token_kept: true }], ..french() };
    let mut harness = window(kept, vault.clone(), FakeSink::default());
    wait_for(&mut harness, "the password store", |h| h.state().can_keep_tokens().is_some());
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the refusal", |h| has(h, "Le jeton gardé ne fonctionne plus"));
    assert_eq!(vault.addresses(), vec![running.address.clone()], "not deleted without a word");
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert_eq!(vault.get(&running.address).unwrap().unwrap().expose(), ADMIN, "replaced once the new one works");
}

#[test]
fn without_a_password_store_nothing_is_kept() {
    let running = start("nostore");
    let vault = Arc::new(Unavailable("no Secret Service on this session".into()));
    let mut harness = window(french(), vault, FakeSink::default());
    wait_for(&mut harness, "the password store", |h| h.state().can_keep_tokens().is_some());
    assert!(has(&harness, "Votre système n'a pas de gestionnaire de mots de passe que l'éditeur puisse utiliser"));
    assert!(has(&harness, "no Secret Service on this session"));
    assert!(harness.get_by_label("Retenir sur cet ordinateur").accesskit_node().is_disabled());
    type_into(&mut harness, "Adresse du serveur", &running.address);
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert!(!harness.state().prefs.servers[0].token_kept);
}

#[test]
fn a_token_the_store_no_longer_has() {
    // Removed from the store by hand (the system's password manager): said, and the field comes back.
    let kept =
        Prefs { servers: vec![SavedServer { address: "http://127.0.0.1:9".into(), token_kept: true }], ..french() };
    let mut harness = window(kept, Arc::new(MemoryVault::default()), FakeSink::default());
    wait_for(&mut harness, "the password store", |h| h.state().can_keep_tokens().is_some());
    assert!(has(&harness, "Jeton gardé dans"));
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the notice", |h| has(h, "Aucun jeton n'est plus gardé pour ce serveur"));
    assert!(!harness.state().prefs.servers[0].token_kept);
    assert_eq!(harness.query_all_by_role(Role::PasswordInput).count(), 1);
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
    wait_for(&mut harness, "the start page", |h| has(h, "Adresse et jeton de gestion"));
}
