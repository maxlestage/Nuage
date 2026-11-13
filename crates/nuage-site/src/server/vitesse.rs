//! The same hook on Vitesse's types (feature `vitesse`).

use std::sync::Arc;

use super::Site;

/// [`handler`](super::handler::handler) for a Vitesse server: `Some(response)` for the
/// site's pages and files, `None` for the rest.
///
/// ```no_run
/// use vitesse::prelude::*;
///
/// let site = nuage_site::vitesse_handler();
/// let mut app = App::new();
/// app.fallback(move |req: Request| {
///     let page = site(&req);
///     async move { page.unwrap_or_else(|| res::status(404)) }
/// });
/// ```
pub fn vitesse_handler() -> impl Fn(&vitesse::Request) -> Option<vitesse::Response> + Clone + Send + Sync + 'static {
    let site = Arc::new(Site::new());
    move |req: &vitesse::Request| {
        let response = site.respond(req.method(), req.uri(), req.headers())?;
        Some(vitesse::Response::from_http(response.map(vitesse::Body::from)))
    }
}
