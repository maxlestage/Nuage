//! The editor as a person uses it, against a real Nuage DB server running in
//! this process: the window is driven headless through its accessibility
//! tree (labels, clicks, typing), in French.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use nuage::{Engine, EngineOptions, Server, ServerOptions};
use nuage_editor::api::{self, ApiError};
use nuage_editor::i18n::Lang;
use nuage_editor::{Editor, Prefs};

const ADMIN: &str = "0123456789abcdef0123456789abcdef-editor";

/// A server in this process, with a management token and an application,
/// as the binary builds it (its environment says the cache budget).
struct Running {
    _runtime: tokio::runtime::Runtime,
    _running: nuage::server::RunningServer,
    address: String,
    server: std::sync::Arc<Server>,
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
    let server = std::sync::Arc::new(Server::new(engine, options));
    server.create_app("shop", "La boutique").unwrap();
    let running = runtime.block_on(server.listen("127.0.0.1:0")).unwrap();
    let address = format!("http://{}", running.local_addr());
    Running { _runtime: runtime, _running: running, address, server }
}

/// Steps the window until `done` holds (calls run on threads of their own).
fn wait_for(harness: &mut Harness<'_, Editor>, what: &str, done: impl Fn(&Harness<'_, Editor>) -> bool) {
    let started = Instant::now();
    while !done(harness) {
        assert!(started.elapsed() < Duration::from_secs(20), "waited too long for: {what}");
        std::thread::sleep(Duration::from_millis(20));
        harness.step();
    }
    harness.run();
}

fn has(harness: &Harness<'_, Editor>, label: &str) -> bool {
    harness.query_all_by_label_contains(label).next().is_some()
}

/// Types `text` in the field named `label`, in place of what it holds.
fn type_into(harness: &mut Harness<'_, Editor>, label: &str, text: &str) {
    harness.get_by_label(label).click();
    harness.run();
    harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    harness.run();
    harness.get_by_label(label).type_text(text);
    harness.run();
}

/// Clicks the button named `label` (a text of the same words may be elsewhere on the page).
fn click(harness: &mut Harness<'_, Editor>, label: &str) {
    harness.get_by_role_and_label(Role::Button, label).click();
    harness.run();
}

#[test]
fn the_api_as_the_editor_calls_it() {
    let running = start("api");
    let fr = api::Server::new(&running.address, ADMIN, Lang::Fr);
    let settings = fr.settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().source, "environment");
    assert!(settings.applications.iter().any(|a| a.name == "shop"));
    // A refusal comes back in the editor's language, with its figures.
    match fr.save_settings(&[("memory.cache_mb", Some("1".into()))]) {
        Err(ApiError::Refused(m)) => assert!(m.contains("trop peu"), "{m}"),
        other => panic!("{other:?}"),
    }
    let saved = fr.save_settings(&[("memory.cache_mb", Some("32 Mio".into()))]).unwrap();
    assert_eq!(saved.get("memory.cache_mb").unwrap().value.as_deref(), Some("32"));
    let back = fr.reset_setting("memory.cache_mb").unwrap();
    assert_eq!(back.get("memory.cache_mb").unwrap().source, "environment");
    // Applications and their tokens.
    let (app, connection) = fr.create_application("blog", "").unwrap();
    assert_eq!(app.name, "blog");
    assert!(connection.token.starts_with("nuage_") && connection.env_lines().contains(&running.address));
    let reader = fr.new_token("blog", true).unwrap();
    let apps = fr.applications().unwrap();
    let blog = apps.iter().find(|a| a.name == "blog").unwrap();
    assert_eq!(blog.tokens.len(), 2);
    let id = blog.tokens.iter().find(|t| t.read_only).unwrap().id.clone();
    assert!(reader.token.contains(&id));
    fr.revoke_token("blog", &id).unwrap();
    let app = fr.set_memory("blog", Some("8 Mio"), Some("auto")).unwrap();
    assert_eq!((app.memory.cache_mb, app.memory.kv_mb), (Some(8), None));
    assert!(matches!(fr.set_memory("nothing", Some("8"), None), Err(ApiError::NotFound(_))));
    // A wrong token, an application's token, an address that is no server.
    let wrong = api::Server::new(&running.address, "first-token", Lang::En).settings().unwrap_err();
    assert!(matches!(wrong, ApiError::Unauthorized(_)), "{wrong:?}");
    let site = api::Server::new(&format!("{}/admin", running.address), ADMIN, Lang::En);
    assert_eq!(site.address(), running.address, "a page of the server is the server");
    assert!(running.server.apps().iter().any(|a| a.name == "blog"));
}

#[test]
fn a_person_connects_and_changes_the_memory_in_french() {
    let running = start("window");
    let prefs = Prefs { lang: Some(Lang::Fr), ..Prefs::default() };
    let mut harness = Harness::builder()
        .with_size([1100.0, 1400.0])
        // A new window (a dialog) settles in a few frames: its widgets are styled once they exist.
        .with_max_steps(30)
        .build_ui_state(|ui, editor: &mut Editor| editor.show(ui), Editor::new(prefs, None));
    harness.run();
    assert!(has(&harness, "Se connecter à votre serveur Nuage DB"));

    // Nothing typed: the window says what is missing.
    harness.get_by_label("Se connecter").click();
    harness.run();
    assert!(has(&harness, "Tapez d'abord l'adresse du serveur"));

    // A wrong token: refused, and the window says which token it needs.
    type_into(&mut harness, "Adresse du serveur", &running.address);
    type_into(&mut harness, "Jeton de gestion", "first-token");
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the refusal", |h| has(h, "Le serveur a refusé ce jeton"));

    // The right one.
    type_into(&mut harness, "Jeton de gestion", ADMIN);
    harness.get_by_label("Se connecter").click();
    wait_for(&mut harness, "the connection", |h| h.state().connected_to().is_some());
    assert_eq!(harness.state().connected_to(), Some(running.address.as_str()));
    assert_eq!(harness.state().prefs.servers, vec![running.address.clone()], "remembered (the address only)");
    wait_for(&mut harness, "the overview", |h| has(h, "répond, et son jeton de gestion est le bon"));

    // Memory: the cache budget of the server, written the French way.
    click(&mut harness, "Mémoire");
    assert!(has(&harness, "Vient de l'environnement (NUAGE_CACHE_MB=48)"));
    type_into(&mut harness, "Caches de pages, toutes les applications ensemble", "32 Mio");
    harness.get_by_label("Enregistrer la mémoire").click();
    wait_for(&mut harness, "the save", |h| has(h, "en vigueur tout de suite"));
    let settings = api::Server::new(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().value.as_deref(), Some("32"));
    assert_eq!(settings.memory.cache_budget_bytes, Some(32 << 20));
    wait_for(&mut harness, "the source line", |h| has(h, "Enregistré ici : il l'emporte sur l'environnement"));

    // Given back to the environment, with one click.
    harness.get_by_label("Revenir à la valeur de l'environnement").click();
    wait_for(&mut harness, "the reset", |h| has(h, "Vient de l'environnement (NUAGE_CACHE_MB=48)"));
    let settings = api::Server::new(&running.address, ADMIN, Lang::En).settings().unwrap();
    assert_eq!(settings.get("memory.cache_mb").unwrap().source, "environment");

    // Too much for one application: refused in French, with the figures.
    type_into(&mut harness, "shop · Cache de pages", "2 Go");
    harness.get_by_label("Enregistrer · shop").click();
    wait_for(&mut harness, "the refusal", |h| has(h, "c'est plus que ce qui reste"));
}

#[test]
fn a_person_makes_an_application_and_copies_its_token() {
    let running = start("apps");
    let prefs = Prefs { lang: Some(Lang::Fr), ..Prefs::default() };
    let mut harness = Harness::builder()
        .with_size([1100.0, 1600.0])
        .with_max_steps(30)
        .build_ui_state(|ui, editor: &mut Editor| editor.show(ui), Editor::new(prefs, None));
    harness.run();
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
    // The token opens the new application, and only it.
    let token = lines.lines().find_map(|l| l.strip_prefix("NUAGE_TOKEN=")).unwrap().to_owned();
    assert!(running.server.apps().iter().any(|a| a.name == "blog"));
    harness.get_by_label("Je l'ai copié : fermer").click();
    harness.run();
    assert!(!has(&harness, "Copiez-le maintenant"), "shown once");
    assert!(!format!("{:?}", harness.state().connected_to()).contains(&token));

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
