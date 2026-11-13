//! The hook the Nuage DB server mounts: a closure from a request to the site's response, on
//! the `http` crate's types (the ones hyper, axum and Vitesse use), independent of the
//! engine.

use std::sync::Arc;

pub use super::Site;

/// The site as a handler: `Some(response)` for the site's pages and files, `None` for what
/// is not the site's (other methods than `GET`/`HEAD`, `/api/*`, `/admin*`), which the
/// server then answers itself.
///
/// Everything is embedded and rendered from memory; the closure is cheap to clone and to
/// call from any thread. The public address comes from `SITE_URL` when set, else from each
/// request's `Host` (and `X-Forwarded-Host` / `X-Forwarded-Proto` behind a proxy).
///
/// ```no_run
/// let site = nuage_site::handler();
/// let request = http::Request::get("/app").body(()).unwrap();
/// let response = site(&request).unwrap();
/// assert_eq!(response.status(), 302); // to /en/app/
/// ```
pub fn handler<B>() -> impl Fn(&http::Request<B>) -> Option<http::Response<Vec<u8>>> + Clone + Send + Sync + 'static {
    let site = Arc::new(Site::new());
    move |req: &http::Request<B>| site.respond(req.method(), req.uri(), req.headers())
}
