#!/usr/bin/env bash
# Builds the `nuage` binary (the Nuage DB server, with the website and the live dashboard
# of /app) for Heroku: run by RustConfig once the emk/rust buildpack has installed Rust.
# Works by hand too, from the repository: ./heroku-build.sh
# The Procfile starts target/release/nuage.
set -euo pipefail
cd "$(dirname "$0")"
# On Heroku, Cargo builds into the buildpack's cache (CARGO_TARGET_DIR).
target="${CARGO_TARGET_DIR:-target}"

# The browser bundle of the dashboard is committed (crates/nuage-site/pkg, rebuilt by
# crates/nuage-site/build-wasm.sh): the server embeds it, no WebAssembly build here.
if [ ! -f crates/nuage-site/pkg/nuage_site_bg.wasm ]; then
  echo "       crates/nuage-site/pkg has no browser bundle: run crates/nuage-site/build-wasm.sh" >&2
  exit 1
fi

echo "-----> Building Nuage DB (server + website)"
cargo build --release --locked -p nuage --bin nuage --features site
mkdir -p target/release
if [ "$target" != target ]; then
  cp "$target/release/nuage" target/release/
fi
echo "       target/release/nuage ready"
