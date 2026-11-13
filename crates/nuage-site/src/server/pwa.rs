//! Installable and offline: the web app manifest of each language (built with
//! `active::pwa::Manifest`, plus shortcuts and screenshots) and the service worker.
//!
//! The service worker is the one active generates for the other sites of the ecosystem,
//! with three changes: files are served from the cache first (they only change with a
//! deployment, which installs a new worker and drops the old caches), a new worker waits
//! until the page asks it to take over (the "new version available" prompt), and `/api/`
//! and `/admin` are never cached.

use active::pwa::Manifest;
use serde_json::{Value, json};

use super::assets::{Assets, fnv};
use crate::i18n::t;
use crate::routes::{Lang, Page, href};
use crate::{THEME_DARK, THEME_LIGHT};

/// Paths the service worker never touches: the status API and the console.
pub const NETWORK_ONLY: [&str; 2] = ["/api/", "/admin"];

/// The PNG icons: (file, size, purpose).
pub const ICONS: [(&str, u32, &str); 4] = [
    ("/static/icon-192.png", 192, "any"),
    ("/static/icon-512.png", 512, "any"),
    ("/static/icon-maskable-192.png", 192, "maskable"),
    ("/static/icon-maskable-512.png", 512, "maskable"),
];

/// The screenshots of the install dialog: (file, width, height, form factor).
pub const SCREENSHOTS: [(&str, u32, u32, &str); 2] =
    [("/static/screenshot-wide.png", 1280, 800, "wide"), ("/static/screenshot-narrow.png", 780, 1688, "narrow")];

/// `/{lang}/manifest.webmanifest`. One identity (`id`) for every language; each opens on
/// its own home page.
pub fn manifest(lang: Lang) -> String {
    let tx = t(lang);
    let mut manifest = Manifest::new("Nuage DB")
        .short_name("Nuage DB")
        .description(tx.meta_description)
        .id("/")
        .start_url(href(lang, Page::Home))
        .scope("/")
        .display("standalone")
        .theme_color(THEME_DARK)
        .background_color(THEME_DARK)
        .lang(lang.code())
        .categories(["developer", "productivity", "utilities"]);
    for (src, size, purpose) in ICONS {
        let sizes = format!("{size}x{size}");
        manifest = if purpose == "maskable" {
            manifest.maskable_icon(src, sizes, "image/png")
        } else {
            manifest.icon(src, sizes, "image/png")
        };
    }
    manifest = manifest.icon("/static/favicon.svg", "any", "image/svg+xml");
    let mut value: Value = serde_json::from_str(&manifest.to_json()).expect("active writes valid JSON");
    let shortcut = |name: &str, short: &str, page: Page, icon: &str| {
        json!({
            "name": name,
            "short_name": short,
            "url": href(lang, page),
            "icons": [{ "src": icon, "sizes": "96x96", "type": "image/png" }],
        })
    };
    let object = value.as_object_mut().expect("an object");
    object.insert("dir".into(), json!("ltr"));
    object.insert("prefer_related_applications".into(), json!(false));
    object.insert(
        "shortcuts".into(),
        json!([
            shortcut(tx.nav_status, "App", Page::App, "/static/shortcut-app.png"),
            shortcut(tx.docs_title, "Docs", Page::Docs, "/static/shortcut-docs.png"),
        ]),
    );
    object.insert(
        "screenshots".into(),
        Value::Array(
            SCREENSHOTS
                .iter()
                .map(|(src, w, h, form)| {
                    json!({
                        "src": src,
                        "sizes": format!("{w}x{h}"),
                        "type": "image/png",
                        "form_factor": form,
                        "label": format!("{} · Nuage DB", tx.app_title),
                    })
                })
                .collect(),
        ),
    );
    value.to_string()
}

/// The `<head>` tags of an installable page, for every platform.
pub fn head_tags(lang: Lang) -> String {
    format!(
        concat!(
            r#"<link rel="manifest" href="/{lang}/manifest.webmanifest"/>"#,
            r#"<meta name="theme-color" content="{light}" media="(prefers-color-scheme: light)"/>"#,
            r#"<meta name="theme-color" content="{dark}"/>"#,
            r#"<meta name="color-scheme" content="dark light"/>"#,
            r#"<meta name="application-name" content="Nuage DB"/>"#,
            r#"<meta name="mobile-web-app-capable" content="yes"/>"#,
            r#"<meta name="apple-mobile-web-app-capable" content="yes"/>"#,
            r#"<meta name="apple-mobile-web-app-title" content="Nuage DB"/>"#,
            r#"<meta name="apple-mobile-web-app-status-bar-style" content="black-translucent"/>"#,
            r#"<meta name="format-detection" content="telephone=no"/>"#,
            r#"<link rel="apple-touch-icon" sizes="180x180" href="/static/apple-touch-icon.png"/>"#,
            r#"<link rel="icon" type="image/svg+xml" href="/static/favicon.svg"/>"#,
            r#"<link rel="icon" type="image/png" sizes="32x32" href="/static/favicon-32.png"/>"#,
            r#"<link rel="icon" type="image/png" sizes="16x16" href="/static/favicon-16.png"/>"#,
        ),
        lang = lang.code(),
        light = THEME_LIGHT,
        dark = THEME_DARK,
    )
}

/// The offline page, shown for pages never opened while the network is down.
pub const OFFLINE_URL: &str = "/offline/";

/// Every URL downloaded when the service worker installs, exactly as the pages request
/// them (versioned files with their `?v=`): the home page and the dashboard of every
/// language, the offline page, the stylesheet, the fonts, the icons and the browser bundle.
pub fn precache(assets: &Assets) -> Vec<String> {
    let mut urls = vec![OFFLINE_URL.to_string()];
    for lang in Lang::ALL {
        urls.push(href(lang, Page::Home));
        urls.push(href(lang, Page::App));
    }
    for (url, _) in assets.iter() {
        let bundle = url.starts_with("/pkg/") && (url.ends_with(".js") || url.ends_with(".wasm"));
        if url == "/static/app.css" || bundle {
            urls.push(assets.url(url));
        } else if url.starts_with("/static/fonts/") && url.ends_with(".woff2")
            || url.starts_with("/static/icon-")
            || url.starts_with("/static/favicon")
            || url == "/static/apple-touch-icon.png"
        {
            urls.push(url.to_string());
        }
    }
    urls
}

/// A version that changes with every deployment that changes a file or a page.
pub fn version(assets: &Assets, pages: u64) -> String {
    let mut h = fnv(0xcbf2_9ce4_8422_2325, env!("CARGO_PKG_VERSION").as_bytes());
    for (url, asset) in assets.iter() {
        h = fnv(h, url.as_bytes());
        h = fnv(h, asset.version.as_bytes());
    }
    h = fnv(h, &pages.to_le_bytes());
    format!("{h:016x}")[..12].to_string()
}

/// JSON string literal that cannot close a `<script>` element.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).expect("a string").replace('<', "\\u003c")
}

/// `/sw.js`.
pub fn service_worker(assets: &Assets, version: &str) -> String {
    let list = |urls: &[String]| format!("[{}]", urls.iter().map(|u| js_string(u)).collect::<Vec<_>>().join(","));
    SCRIPT
        .replace("__CACHE__", &js_string(&format!("nuage-site-{version}")))
        .replace("__PREFIX__", &js_string("nuage-site-"))
        .replace("__PRECACHE__", &list(&precache(assets)))
        .replace("__NETWORK_ONLY__", &list(&NETWORK_ONLY.map(String::from)))
        .replace("__OFFLINE__", &js_string(OFFLINE_URL))
}

const SCRIPT: &str = r#"// Service worker of the Nuage DB website (generated by nuage-site, after active's).
const CACHE = __CACHE__;
const PREFIX = __PREFIX__;
const PRECACHE = __PRECACHE__;
const NETWORK_ONLY = __NETWORK_ONLY__;
const OFFLINE = __OFFLINE__;

self.addEventListener("install", (event) => {
  // The new version waits: the page offers to reload (see "message").
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(PRECACHE)));
});

self.addEventListener("message", (event) => {
  if (event.data === "skip-waiting") self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(
        keys.filter((key) => key.startsWith(PREFIX) && key !== CACHE).map((key) => caches.delete(key)),
      ))
      .then(() => self.clients.claim()),
  );
});

function keep(request, response) {
  if (response.ok && response.type === "basic") {
    const copy = response.clone();
    caches.open(CACHE).then((cache) => cache.put(request, copy));
  }
  return response;
}

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  // The status API, its live stream and the console: always the network, never cached.
  if (NETWORK_ONLY.some((prefix) => url.pathname.startsWith(prefix))) return;
  if (request.headers.get("accept") === "text/event-stream") return;

  // Pages: the network first, the cached copy offline, else the offline page.
  if (request.mode === "navigate") {
    event.respondWith(
      fetch(request)
        .then((response) => keep(request, response))
        .catch(() => caches.match(request)
          .then((cached) => cached || caches.match(OFFLINE))
          .then((response) => response || Response.error())),
    );
    return;
  }

  // Files: the cache first.
  event.respondWith(
    caches.match(request).then((cached) => cached || fetch(request).then((response) => keep(request, response))),
  );
});
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_opens_each_language_on_its_home_page() {
        let json: Value = serde_json::from_str(&manifest(Lang::Fr)).unwrap();
        assert_eq!(json["start_url"], "/fr/");
        assert_eq!(json["scope"], "/");
        assert_eq!(json["display"], "standalone");
        assert_eq!(json["shortcuts"][0]["url"], "/fr/app/");
        assert_eq!(json["screenshots"][1]["form_factor"], "narrow");
    }

    #[test]
    fn the_worker_never_caches_the_api_nor_the_console() {
        let js = service_worker(&Assets::embedded(), "v1");
        assert!(js.contains(r#"const NETWORK_ONLY = ["/api/","/admin"];"#), "{js}");
        assert!(!js.contains("\"/api/status\""));
        assert!(js.contains(r#"const OFFLINE = "/offline/";"#));
        assert!(js.contains("\"/en/app/\""));
    }
}
