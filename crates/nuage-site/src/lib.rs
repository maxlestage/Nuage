//! The website of Nuage DB, written with [active](https://active-c1883a966650.herokuapp.com/):
//! server-rendered pages in English, French and Spanish, the live status dashboard of
//! `/app` (a Rust/WebAssembly island), and a complete PWA (manifest, icons, offline service
//! worker with an update prompt).
//!
//! The library is shared by both sides. Compiled for the server it renders the pages and
//! serves every file, embedded in the binary: the Nuage DB server mounts it with
//! [`handler`] (or [`vitesse_handler`] with the `vitesse` feature). Compiled to WebAssembly
//! (`build-wasm.sh`) it is the browser bundle, which hydrates the dashboard.
//!
//! ```no_run
//! # #[cfg(not(target_arch = "wasm32"))] {
//! let site = nuage_site::handler();
//! let request = http::Request::get("/en/").body(()).unwrap();
//! let response = site(&request).expect("a page of the site");
//! assert_eq!(response.status(), 200);
//! # }
//! ```

pub mod dashboard;
pub mod i18n;
pub mod icons;
pub mod routes;
pub mod status;

/// The theme color (dark, the default): status bar, splash screen, `theme-color`.
pub const THEME_DARK: &str = "#060913";
/// The theme color of the light theme.
pub const THEME_LIGHT: &str = "#f5f7fc";
/// The repository of Nuage DB.
pub const GITHUB: &str = "https://github.com/maxlestage/Nuage";
/// The author of Nuage DB and of its sister projects.
pub const AUTHOR: &str = "Maxime Nathan Lestage";

/// Every island, registered under the name used by `active::island`.
pub const ISLANDS: [(&str, active::IslandFn); 1] = [("dashboard", dashboard::dashboard)];

#[cfg(not(target_arch = "wasm32"))]
pub mod server;

#[cfg(not(target_arch = "wasm32"))]
pub use server::handler::{Site, handler};

#[cfg(all(not(target_arch = "wasm32"), feature = "vitesse"))]
pub use server::vitesse::vitesse_handler;

#[cfg(target_arch = "wasm32")]
mod client;
