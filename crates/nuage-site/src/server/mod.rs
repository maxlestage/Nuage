//! The server side: every page and file of the site, answered from memory by
//! [`Site::respond`], on the `http` crate's types so that any Rust HTTP server can mount it
//! ([`handler`](handler::handler), and [`vitesse_handler`](vitesse::vitesse_handler) with the
//! `vitesse` feature).
//!
//! * `GET` and `HEAD` only; `/api/*` and `/admin*` are never answered (they belong to the
//!   Nuage DB server), everything else is: pages, a 404 page for unknown paths;
//! * `/` and the pages without a language (`/app`, `/docs`…) redirect to the visitor's
//!   language (`Accept-Language`, English by default);
//! * files are embedded, compressed once, and cached forever when the URL carries their
//!   `?v=` hash; pages are compressed on the way out;
//! * security headers on every response, and a Content-Security-Policy on pages.

pub mod assets;
pub mod handler;
pub mod highlight;
pub mod layout;
pub mod pages;
pub mod pwa;
pub mod security;
#[cfg(feature = "vitesse")]
pub mod vitesse;

use http::{HeaderMap, HeaderValue, Method, Response, StatusCode, Uri, header};

use crate::routes::{Lang, Page, Route, bare, href};
use assets::{Assets, compress};
use layout::Ctx;

/// The site: its files, its service worker and its public address.
pub struct Site {
    pub assets: Assets,
    /// Public address if configured (`SITE_URL`), else taken from each request.
    pub site_url: Option<String>,
    /// The service worker (it depends on every file and page: built once).
    sw: String,
}

impl Default for Site {
    fn default() -> Site {
        Site::new()
    }
}

impl Site {
    /// The site, with its public address from `SITE_URL` (else taken from each request's
    /// `Host`, `X-Forwarded-Host` and `X-Forwarded-Proto`).
    pub fn new() -> Site {
        Site::with_url(std::env::var("SITE_URL").ok())
    }

    /// The site, with its public address (`https://nuage.example.com`) given.
    pub fn with_url(site_url: Option<String>) -> Site {
        let assets = Assets::embedded();
        let mut site = Site {
            assets,
            site_url: site_url.map(|u| u.trim_end_matches('/').to_string()).filter(|u| !u.is_empty()),
            sw: String::new(),
        };
        // The version of the worker follows the files and the pages as rendered.
        let mut pages = 0xcbf2_9ce4_8422_2325_u64;
        for route in site.routes() {
            pages = assets::fnv(pages, pages::render(&Ctx { site: &site, route, origin: "" }).as_bytes());
        }
        pages = assets::fnv(pages, pages::offline(&site).as_bytes());
        let version = pwa::version(&site.assets, pages);
        site.sw = pwa::service_worker(&site.assets, &version);
        site
    }

    /// Every page of the site, in every language.
    pub fn routes(&self) -> Vec<Route> {
        Lang::ALL.into_iter().flat_map(|lang| Page::ALL.map(|page| Route::new(lang, page))).collect()
    }

    /// The service worker served at `/sw.js`.
    pub fn service_worker(&self) -> &str {
        &self.sw
    }

    /// The public origin (`https://example.com`).
    pub fn origin(&self, headers: &HeaderMap) -> String {
        if let Some(url) = &self.site_url {
            return url.clone();
        }
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(',').next().unwrap_or("").trim().to_string())
        };
        let host = header("x-forwarded-host").or_else(|| header("host")).unwrap_or_else(|| "localhost".into());
        if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':')) {
            return "http://localhost".into();
        }
        let proto = if header("x-forwarded-proto").as_deref() == Some("https") { "https" } else { "http" };
        format!("{proto}://{host}")
    }

    /// The answer of the site to a request, `None` for what is not the site's: other methods
    /// than `GET` and `HEAD`, `/api/*` and `/admin*`.
    pub fn respond(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> Option<Response<Vec<u8>>> {
        if method != Method::GET && method != Method::HEAD {
            return None;
        }
        let path = uri.path();
        if is_not_ours(path) {
            return None;
        }
        let response = self.route(path, uri.query(), headers);
        let https = headers.get("x-forwarded-proto").is_some_and(|v| v.as_bytes() == b"https");
        let mut response = response;
        security::headers(response.headers_mut(), https);
        if method == Method::HEAD {
            response.body_mut().clear();
        }
        Some(response)
    }

    fn route(&self, path: &str, query: Option<&str>, headers: &HeaderMap) -> Response<Vec<u8>> {
        let accept = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
        match path {
            "/sw.js" => {
                return text(self.sw.as_bytes().to_vec(), "text/javascript; charset=utf-8", "no-cache", accept);
            }
            "/robots.txt" => {
                let body = format!(
                    "User-agent: *\nAllow: /\nDisallow: /admin\nDisallow: /api/\n\nSitemap: {}/sitemap.xml\n",
                    self.origin(headers)
                );
                return text(body.into_bytes(), "text/plain; charset=utf-8", "public, max-age=3600", accept);
            }
            "/sitemap.xml" => {
                return text(
                    self.sitemap(headers).into_bytes(),
                    "application/xml; charset=utf-8",
                    "public, max-age=3600",
                    accept,
                );
            }
            "/offline" | "/offline/" => return self.html(200, pages::offline(self), accept),
            "/favicon.ico" => return self.file("/static/favicon-32.png", query, accept),
            "/favicon.svg" | "/apple-touch-icon.png" | "/og-image.png" => {
                return self.file(&format!("/static{path}"), query, accept);
            }
            _ => {}
        }
        if path.starts_with("/static/") || path.starts_with("/pkg/") {
            return self.file(path, query, accept);
        }
        let manifest_lang = path
            .strip_prefix('/')
            .and_then(|p| p.strip_suffix("/manifest.webmanifest"))
            .and_then(|code| Lang::ALL.into_iter().find(|l| l.code() == code));
        if let Some(lang) = manifest_lang {
            return text(pwa::manifest(lang).into_bytes(), "application/manifest+json", "public, max-age=3600", accept);
        }
        let route = match Route::parse(path) {
            Some(route) => route,
            None => match bare(path) {
                Some(page) => return redirect_to_lang(headers, page, query),
                None => Route::new(pick_lang(headers), Page::NotFound),
            },
        };
        if route.page != Page::NotFound && !path.ends_with('/') {
            let query = query.map(|q| format!("?{q}")).unwrap_or_default();
            return redirect(StatusCode::MOVED_PERMANENTLY, &format!("{path}/{query}"), false);
        }
        let origin = self.origin(headers);
        let body = pages::render(&Ctx { site: self, route, origin: &origin });
        let status = if route.page == Page::NotFound { 404 } else { 200 };
        self.html(status, body, accept)
    }

    fn html(&self, status: u16, html: String, accept: Option<&str>) -> Response<Vec<u8>> {
        let csp = security::csp(&html);
        let mut response = text(html.into_bytes(), "text/html; charset=utf-8", "no-cache", accept);
        *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
        if let Ok(csp) = HeaderValue::from_str(&csp) {
            response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, csp);
        }
        response
    }

    /// An embedded file, compressed in advance when the browser accepts it. Versioned URLs
    /// (`?v=`) never change: browsers keep them a year.
    fn file(&self, url: &str, query: Option<&str>, accept: Option<&str>) -> Response<Vec<u8>> {
        let Some(asset) = self.assets.get(url) else {
            let mut response = Response::new(b"Not found".to_vec());
            *response.status_mut() = StatusCode::NOT_FOUND;
            set(&mut response, header::CONTENT_TYPE, "text/plain; charset=utf-8");
            set(&mut response, header::CACHE_CONTROL, "no-cache");
            return response;
        };
        let cache = if query.is_some_and(|q| q.starts_with("v=")) {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=3600"
        };
        let brotli = if accepts(accept, "br") { asset.brotli() } else { None };
        let gzip = if brotli.is_none() && accepts(accept, "gzip") { asset.gzip() } else { None };
        let (body, encoding) = match (brotli, gzip) {
            (Some(br), _) => (br, Some("br")),
            (None, Some(gz)) => (gz, Some("gzip")),
            _ => (asset.bytes, None),
        };
        let mut response = Response::new(body.to_vec());
        set(&mut response, header::CONTENT_TYPE, asset.content_type);
        set(&mut response, header::CACHE_CONTROL, cache);
        set(&mut response, header::VARY, "Accept-Encoding");
        if let Some(encoding) = encoding {
            set(&mut response, header::CONTENT_ENCODING, encoding);
        }
        response
    }

    fn sitemap(&self, headers: &HeaderMap) -> String {
        let origin = self.origin(headers);
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" xmlns:xhtml=\"http://www.w3.org/1999/xhtml\">\n",
        );
        for route in self.routes() {
            xml.push_str(&format!("<url><loc>{origin}{}</loc>", route.path()));
            for lang in Lang::ALL {
                xml.push_str(&format!(
                    r#"<xhtml:link rel="alternate" hreflang="{}" href="{origin}{}"/>"#,
                    lang.code(),
                    route.with_lang(lang).path()
                ));
            }
            xml.push_str("</url>\n");
        }
        xml.push_str("</urlset>\n");
        xml
    }
}

/// `/api/*` and `/admin*`: the Nuage DB server's, never the site's.
pub fn is_not_ours(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path.starts_with("/admin")
}

fn set(response: &mut Response<Vec<u8>>, name: header::HeaderName, value: &'static str) {
    response.headers_mut().insert(name, HeaderValue::from_static(value));
}

/// A text response, compressed when the browser accepts it.
fn text(body: Vec<u8>, content_type: &'static str, cache: &'static str, accept: Option<&str>) -> Response<Vec<u8>> {
    let (body, encoding) = if body.len() < 1024 {
        (body, None)
    } else if accepts(accept, "br") {
        match compress(&body, true, false) {
            Some(br) => (br, Some("br")),
            None => (body, None),
        }
    } else if accepts(accept, "gzip") {
        match compress(&body, false, false) {
            Some(gz) => (gz, Some("gzip")),
            None => (body, None),
        }
    } else {
        (body, None)
    };
    let mut response = Response::new(body);
    set(&mut response, header::CONTENT_TYPE, content_type);
    set(&mut response, header::CACHE_CONTROL, cache);
    set(&mut response, header::VARY, "Accept-Encoding");
    if let Some(encoding) = encoding {
        set(&mut response, header::CONTENT_ENCODING, encoding);
    }
    response
}

fn redirect(status: StatusCode, location: &str, varies: bool) -> Response<Vec<u8>> {
    let mut response = Response::new(Vec::new());
    *response.status_mut() = status;
    if let Ok(location) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    if varies {
        set(&mut response, header::VARY, "Accept-Language");
        set(&mut response, header::CACHE_CONTROL, "no-cache");
    }
    response
}

/// A redirect to a page in the visitor's language (it depends on the visitor).
fn redirect_to_lang(headers: &HeaderMap, page: Page, query: Option<&str>) -> Response<Vec<u8>> {
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    redirect(StatusCode::FOUND, &format!("{}{query}", href(pick_lang(headers), page)), true)
}

/// The visitor's language: `Accept-Language`, English by default. The site sets no cookie.
pub fn pick_lang(headers: &HeaderMap) -> Lang {
    lang_of(headers.get(header::ACCEPT_LANGUAGE).and_then(|v| v.to_str().ok()).unwrap_or(""))
}

fn lang_of(accept: &str) -> Lang {
    let mut ranked: Vec<(Lang, f32)> = accept
        .split(',')
        .filter_map(|part| {
            let mut pieces = part.trim().split(";q=");
            let lang = Lang::parse(pieces.next()?)?;
            let q = pieces.next().and_then(|q| q.trim().parse().ok()).unwrap_or(1.0);
            Some((lang, q))
        })
        .filter(|(_, q)| *q > 0.0)
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.first().map_or(Lang::En, |r| r.0)
}

/// Whether the `Accept-Encoding` header `accept` accepts `coding` (and does not refuse it
/// with `q=0`).
fn accepts(accept: Option<&str>, coding: &str) -> bool {
    accept.is_some_and(|accept| {
        accept.split(',').any(|part| {
            let mut pieces = part.trim().split(';');
            pieces.next().is_some_and(|c| c.trim().eq_ignore_ascii_case(coding))
                && !pieces.any(|p| matches!(p.trim(), "q=0" | "q=0.0" | "q=0.00" | "q=0.000"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_visitor_language() {
        assert_eq!(lang_of("de-DE,es;q=0.8,en;q=0.5"), Lang::Es);
        assert_eq!(lang_of("fr-CA"), Lang::Fr);
        assert_eq!(lang_of(""), Lang::En);
    }

    #[test]
    fn reads_accept_encoding() {
        assert!(accepts(Some("gzip, deflate, br"), "br"));
        assert!(!accepts(Some("gzip, br;q=0"), "br"));
        assert!(!accepts(None, "gzip"));
    }

    #[test]
    fn leaves_the_api_and_the_console_alone() {
        assert!(is_not_ours("/api/status"));
        assert!(is_not_ours("/admin"));
        assert!(is_not_ours("/admin/login"));
        assert!(!is_not_ours("/app"));
        assert!(!is_not_ours("/apidocs"));
    }
}
