//! The website of Nuage DB (the `nuage-site` crate), served by the server next to the
//! database: the pages in English, French and Spanish, the live dashboard of `/app` (which
//! reads `/api/status` and `/api/status/stream` on the same origin), the PWA and the share
//! card. Every file is embedded in the binary.

use std::sync::Arc;

use hyper::Method;
use hyper::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use hyper::http::Uri;

use crate::server::{SiteHandler, SiteRequest, SiteResponse};

/// The website as a [`SiteHandler`], for [`ServerOptions::site`](crate::ServerOptions::site):
///
/// ```no_run
/// let options = nuage::ServerOptions { site: Some(nuage::site()), ..Default::default() };
/// ```
pub fn site() -> Arc<dyn SiteHandler> {
    Arc::new(Site(nuage_site::Site::new()))
}

/// [`nuage_site::Site`] on the server's request and response types.
struct Site(nuage_site::Site);

impl SiteHandler for Site {
    fn handle(&self, request: &SiteRequest) -> Option<SiteResponse> {
        let uri: Uri = match &request.query {
            Some(query) => format!("{}?{query}", request.path),
            None => request.path.clone(),
        }
        .parse()
        .ok()?;
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
                headers.append(name, value);
            }
        }
        // The server hands over `GET` and `HEAD` only; hyper leaves the body out of a `HEAD`
        // answer by itself.
        let response = self.0.respond(&Method::GET, &uri, &headers)?;
        let (parts, body) = response.into_parts();
        let content_type = parts.headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        let headers = parts
            .headers
            .iter()
            .filter(|(name, _)| *name != CONTENT_TYPE)
            .filter_map(|(name, value)| Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned())))
            .collect();
        Some(SiteResponse { status: parts.status.as_u16(), content_type, headers, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str, query: Option<&str>) -> Option<SiteResponse> {
        site().handle(&SiteRequest {
            path: path.into(),
            query: query.map(Into::into),
            headers: vec![("host".into(), "nuage.test".into()), ("accept-language".into(), "fr-FR,fr;q=0.9".into())],
        })
    }

    fn header<'a>(response: &'a SiteResponse, name: &str) -> Option<&'a str> {
        response.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn pages_and_files() {
        let home = get("/", None).expect("the home page is the site's");
        assert!(matches!(home.status, 301..=308), "{}", home.status);
        assert_eq!(header(&home, "location"), Some("/fr/?lang=auto"));
        let page = get("/fr/app/", None).unwrap();
        assert_eq!(page.status, 200);
        assert!(page.content_type.starts_with("text/html"), "{}", page.content_type);
        assert!(header(&page, "content-security-policy").is_some());
        assert!(String::from_utf8_lossy(&page.body).contains("/admin?next="));
        let css = get("/static/app.css", None).unwrap();
        assert_eq!((css.status, css.content_type.starts_with("text/css")), (200, true));
    }

    #[test]
    fn leaves_the_server_its_paths() {
        assert!(get("/api/status", None).is_none());
        assert!(get("/admin", Some("next=/fr/app/")).is_none());
    }
}
