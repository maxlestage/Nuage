//! The site through its handler, as the Nuage DB server mounts it: routes, share card and
//! SEO tags, PWA manifest and service worker, footer links.
#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeSet;
use std::sync::OnceLock;

use http::{Method, Request, Response, StatusCode};
use nuage_site::Site;
use nuage_site::routes::{Lang, Page, Route};
use serde_json::Value;

fn site() -> &'static Site {
    static SITE: OnceLock<Site> = OnceLock::new();
    SITE.get_or_init(|| Site::with_url(None))
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Option<Response<Vec<u8>>> {
    let mut builder = Request::builder().method(method).uri(uri).header("host", "nuage.test");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let req = builder.body(()).unwrap();
    site().respond(req.method(), req.uri(), req.headers())
}

fn get(uri: &str) -> Response<Vec<u8>> {
    request(Method::GET, uri, &[]).unwrap_or_else(|| panic!("{uri}: not answered by the site"))
}

fn text(uri: &str) -> String {
    let response = get(uri);
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    String::from_utf8(response.into_body()).unwrap()
}

fn header<'a>(response: &'a Response<Vec<u8>>, name: &str) -> &'a str {
    response.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn pages() -> Vec<String> {
    site().routes().into_iter().map(|r| r.path()).collect()
}

/// Width and height of a PNG.
fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let be = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    (be(16), be(20))
}

#[test]
fn every_route_answers() {
    for path in pages() {
        assert_eq!(get(&path).status(), StatusCode::OK, "{path}");
    }
    for path in ["/sw.js", "/robots.txt", "/sitemap.xml", "/offline/", "/favicon.ico", "/apple-touch-icon.png"] {
        assert_eq!(get(path).status(), StatusCode::OK, "{path}");
    }
    for lang in Lang::ALL {
        assert_eq!(get(&format!("/{}/manifest.webmanifest", lang.code())).status(), StatusCode::OK);
    }
    assert_eq!(get("/en/nope/").status(), StatusCode::NOT_FOUND);
    assert_eq!(get("/nope").status(), StatusCode::NOT_FOUND);
    assert_eq!(get("/static/nope.css").status(), StatusCode::NOT_FOUND);
    // Without its slash, a page redirects to its address.
    let response = get("/fr/docs");
    assert_eq!(response.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(header(&response, "location"), "/fr/docs/");
}

#[test]
fn root_and_app_lead_to_the_visitor_language() {
    let response = request(Method::GET, "/", &[("accept-language", "de-DE,es;q=0.8,en;q=0.5")]).unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(header(&response, "location"), "/es/");
    let app = request(Method::GET, "/app", &[("accept-language", "fr-FR")]).unwrap();
    assert_eq!(header(&app, "location"), "/fr/app/");
    assert_eq!(header(&get("/app/"), "location"), "/en/app/");
    assert_eq!(header(&get("/docs"), "location"), "/en/docs/");
}

#[test]
fn leaves_the_api_the_console_and_writes_to_the_server() {
    for path in ["/api/status", "/api/status/stream", "/api/health", "/api", "/admin", "/admin/login?next=/"] {
        assert!(request(Method::GET, path, &[]).is_none(), "{path}");
    }
    assert!(request(Method::POST, "/en/", &[]).is_none());
    let head = request(Method::HEAD, "/en/", &[]).unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert!(head.body().is_empty());
}

#[test]
fn every_page_has_its_share_card_and_seo_tags() {
    let png = get("/static/og-image.png");
    assert_eq!(header(&png, "content-type"), "image/png");
    assert_eq!(png_size(png.body()), (1200, 630));
    for path in pages() {
        let route = Route::parse(&path).unwrap();
        let html = text(&path);
        let lang = route.lang.code();
        assert!(html.starts_with(&format!(r#"<!DOCTYPE html><html lang="{lang}""#)), "{path}");
        assert_eq!(html.matches("<title>").count(), 1, "{path}");
        for tag in [
            r#"<meta name="description" content=""#,
            r#"<meta property="og:title" content=""#,
            r#"<meta property="og:description" content=""#,
            &format!(r#"<meta property="og:url" content="http://nuage.test{path}"/>"#),
            &format!(r#"<link rel="canonical" href="http://nuage.test{path}"/>"#),
            r#"<meta property="og:image" content="http://nuage.test/static/og-image.png"/>"#,
            r#"<meta property="og:image:width" content="1200"/>"#,
            r#"<meta property="og:image:height" content="630"/>"#,
            r#"<meta property="og:image:alt" content=""#,
            r#"<meta property="og:site_name" content="Nuage DB"/>"#,
            r#"<meta name="twitter:card" content="summary_large_image"/>"#,
            r#"<meta name="twitter:title" content=""#,
            r#"<meta name="twitter:description" content=""#,
            r#"<meta name="twitter:image" content="http://nuage.test/static/og-image.png"/>"#,
            r##"<meta name="theme-color" content="#060913"/>"##,
            &format!(r#"<link rel="manifest" href="/{lang}/manifest.webmanifest"/>"#),
            r#"<link rel="apple-touch-icon" sizes="180x180" href="/static/apple-touch-icon.png"/>"#,
            r#"<meta name="apple-mobile-web-app-capable" content="yes"/>"#,
            r#"<link rel="icon" type="image/svg+xml" href="/static/favicon.svg"/>"#,
            r#"navigator.serviceWorker.register("/sw.js")"#,
            r#"hreflang="x-default""#,
        ] {
            assert!(html.contains(tag), "{path}: no {tag}");
        }
        assert!(
            html.contains(r#"as="fetch" type="application/wasm""#),
            "{path}: no browser bundle (run build-wasm.sh)"
        );
    }
}

#[test]
fn pages_are_secured_and_compressed() {
    let response =
        request(Method::GET, "/en/", &[("accept-encoding", "gzip, br"), ("x-forwarded-proto", "https")]).unwrap();
    assert_eq!(header(&response, "content-encoding"), "br");
    assert!(header(&response, "strict-transport-security").contains("max-age"));
    let csp = header(&response, "content-security-policy");
    assert!(csp.contains("connect-src 'self'") && csp.contains("'wasm-unsafe-eval'"), "{csp}");
    assert_eq!(header(&response, "x-content-type-options"), "nosniff");
    let css = text("/en/").split("/static/app.css?v=").nth(1).unwrap().split('"').next().unwrap().to_string();
    let file = get(&format!("/static/app.css?v={css}"));
    assert_eq!(header(&file, "cache-control"), "public, max-age=31536000, immutable");
}

#[test]
fn the_manifest_is_complete() {
    for lang in Lang::ALL {
        let body = text(&format!("/{}/manifest.webmanifest", lang.code()));
        let json: Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(json["name"], "Nuage DB");
        assert!(json["short_name"].as_str().is_some_and(|s| !s.is_empty() && s.len() <= 12));
        assert_eq!(json["display"], "standalone");
        assert_eq!(json["start_url"], format!("/{}/", lang.code()));
        assert_eq!(json["scope"], "/");
        assert_eq!(json["id"], "/");
        assert_eq!(json["theme_color"], nuage_site::THEME_DARK);
        assert_eq!(json["background_color"], nuage_site::THEME_DARK);
        assert_eq!(json["lang"], lang.code());
        let icons = json["icons"].as_array().unwrap();
        for (size, purpose) in [(192, "any"), (512, "any"), (192, "maskable"), (512, "maskable")] {
            let icon = icons
                .iter()
                .find(|i| i["sizes"] == format!("{size}x{size}") && i["purpose"] == purpose)
                .unwrap_or_else(|| panic!("no {purpose} {size} icon"));
            let file = get(icon["src"].as_str().unwrap());
            assert_eq!(png_size(file.body()), (size, size), "{icon}");
        }
        let shortcuts = json["shortcuts"].as_array().unwrap();
        let names: Vec<&str> = shortcuts.iter().map(|s| s["short_name"].as_str().unwrap()).collect();
        assert_eq!(names, ["App", "Docs"]);
        assert_eq!(shortcuts[0]["url"], Route::new(lang, Page::App).path());
        for shortcut in shortcuts {
            assert_eq!(get(shortcut["url"].as_str().unwrap()).status(), StatusCode::OK);
            assert_eq!(get(shortcut["icons"][0]["src"].as_str().unwrap()).status(), StatusCode::OK);
        }
        let screenshots = json["screenshots"].as_array().unwrap();
        assert!(screenshots.iter().any(|s| s["form_factor"] == "wide"));
        assert!(screenshots.iter().any(|s| s["form_factor"] == "narrow"));
        for shot in screenshots {
            let file = get(shot["src"].as_str().unwrap());
            let (w, h) = png_size(file.body());
            assert_eq!(shot["sizes"], format!("{w}x{h}"));
        }
    }
    let apple = get("/static/apple-touch-icon.png");
    assert_eq!(png_size(apple.body()), (180, 180));
}

#[test]
fn the_service_worker_never_caches_the_api_or_the_console() {
    let response = get("/sw.js");
    assert_eq!(header(&response, "cache-control"), "no-cache");
    assert!(header(&response, "content-type").starts_with("text/javascript"));
    let js = String::from_utf8(response.into_body()).unwrap();
    let list = |name: &str| -> Vec<String> {
        let line = js.lines().find(|l| l.starts_with(&format!("const {name} = "))).unwrap();
        let json = line.trim_start_matches(&format!("const {name} = ")).trim_end_matches(';');
        serde_json::from_str(json).unwrap()
    };
    assert_eq!(list("NETWORK_ONLY"), ["/api/", "/admin"]);
    let precache = list("PRECACHE");
    assert!(precache.iter().all(|u| !u.starts_with("/api") && !u.starts_with("/admin")), "{precache:?}");
    // The shell: the home page and the dashboard of every language, the offline page, the
    // stylesheet and the bundle, all of which exist.
    for lang in Lang::ALL {
        assert!(precache.contains(&format!("/{}/app/", lang.code())));
    }
    assert!(precache.iter().any(|u| u.starts_with("/pkg/nuage_site_bg.wasm?v=")));
    assert!(precache.contains(&"/offline/".to_string()));
    for url in &precache {
        assert_eq!(get(url).status(), StatusCode::OK, "{url}");
    }
    assert!(js.contains(r#"const OFFLINE = "/offline/";"#));
    assert!(js.contains("event.data === \"skip-waiting\""), "the update flow");
    assert!(js.contains("request.mode === \"navigate\""));
    assert!(!js.contains("caches.open(CACHE).then((cache) => cache.addAll(PRECACHE)).then(() => self.skipWaiting())"));
}

/// Every `href="…"` of `html`.
fn links(html: &str) -> Vec<String> {
    html.split(" href=\"").skip(1).filter_map(|rest| rest.split('"').next()).map(|h| h.replace("&amp;", "&")).collect()
}

fn ids(html: &str) -> BTreeSet<String> {
    html.split(" id=\"").skip(1).filter_map(|rest| rest.split('"').next()).map(String::from).collect()
}

#[test]
fn no_link_is_broken() {
    let allowed_external = [
        nuage_site::GITHUB,
        "https://paquets-90dfc06b6fea.herokuapp.com/",
        "https://vitesse-f16135654c3d.herokuapp.com/",
        "https://active-c1883a966650.herokuapp.com/",
        "https://memoire-7f4525f1fdaf.herokuapp.com/",
        "https://securite-2db4c1994be7.herokuapp.com/",
        "https://github.com/maxlestage/Facile",
        "https://maxlestage.github.io/Rythme/",
        "http://nuage.test/",
    ];
    let mut checked = BTreeSet::new();
    for path in pages().into_iter().chain(["/offline/".to_string()]) {
        let html = text(&path);
        for link in links(&html) {
            if link.starts_with("http") {
                assert!(allowed_external.iter().any(|a| link.starts_with(a)), "{path}: unknown link {link}");
                continue;
            }
            if link.is_empty() {
                continue; // the offline page's "try again"
            }
            // The console belongs to the server (`/admin?next=…` comes back to the dashboard).
            if link.starts_with("/admin") {
                assert!(link == "/admin" || link.starts_with("/admin?next=%2F"), "{link}");
                continue;
            }
            let (target, fragment) = match link.split_once('#') {
                Some((target, fragment)) => (target.to_string(), Some(fragment.to_string())),
                None => (link.clone(), None),
            };
            let target = if target.is_empty() { path.clone() } else { target };
            if !checked.insert(link.clone()) && fragment.is_none() {
                continue;
            }
            let response = get(&target);
            assert_eq!(response.status(), StatusCode::OK, "{path}: broken link {link}");
            if let Some(fragment) = fragment.filter(|f| !f.is_empty()) {
                let page = String::from_utf8(response.into_body()).unwrap();
                assert!(
                    ids(&page).contains(&fragment) || ids(&html).contains(&fragment) && target == path,
                    "{path}: no #{fragment} in {target}"
                );
            }
        }
    }
}

#[test]
fn the_footer_is_complete_in_every_language() {
    for lang in Lang::ALL {
        let html = text(&format!("/{}/", lang.code()));
        let footer = html.split("<footer").nth(1).expect("a <footer>");
        for name in ["Paquets", "Vitesse", "Active", "Memoire", "Securite", "Facile", "Rythme"] {
            assert!(footer.contains(&format!(r#"<span class="eco-name">{name}"#)), "{name}");
        }
        for link in [
            "/app/",
            "/admin",
            "/changelog/",
            "/features/",
            "/docs/",
            "#quick-start",
            "#deploy",
            "/faq/",
            "#license",
            "#privacy",
            nuage_site::GITHUB,
        ] {
            assert!(footer.contains(link), "{lang:?}: {link}");
        }
        assert!(footer.contains("© 2026 Maxime Nathan Lestage."));
        assert!(footer.contains(r#"class="lang-switch""#));
        // Landmarks.
        assert!(
            html.contains("<header")
                && html.contains("<main")
                && html.contains(r#"id="main""#)
                && html.contains("<nav")
        );
    }
}

#[test]
fn no_other_database_is_named_and_no_benchmark_is_claimed() {
    for path in pages() {
        let html = text(&path).to_ascii_lowercase();
        let first = ["ne", "on"].concat();
        for word in [first.as_str(), "postgres", "sqlite", "mysql", "mongodb", "supabase"] {
            assert!(!html.contains(word), "{path}: {word}");
        }
        assert!(!html.contains("faster than") && !html.contains(" ops/s"), "{path}");
    }
}

#[test]
fn the_dashboard_is_an_island() {
    for lang in Lang::ALL {
        let html = text(&format!("/{}/app/", lang.code()));
        assert!(html.contains(&format!(r#"<active-island data-island="dashboard" data-props="{}">"#, lang.code())));
        assert!(html.contains(&format!("/admin?next=%2F{}%2Fapp%2F", lang.code())));
    }
}
