#!/usr/bin/env bash
# Builds the browser bundle of the site (the live dashboard's island) into pkg/, which
# build.rs embeds in the server. Run it after changing src/ (the pages are rendered by the
# server: only the islands need it). Needs the wasm32 target and the wasm-bindgen CLI of
# the version in Cargo.lock:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <the version in Cargo.lock>
set -euo pipefail
cd "$(dirname "$0")"
# The crate is a member of the workspace at the root of the repository: its Cargo.lock and
# target/ are there.
root=../..
target="${CARGO_TARGET_DIR:-$root/target}"
cargo build -p nuage-site --lib --locked --target wasm32-unknown-unknown --profile wasm-release
version=$(grep -A1 '^name = "wasm-bindgen"$' "$root/Cargo.lock" | sed -n 's/^version = "\(.*\)"$/\1/p')
if ! wasm-bindgen --version | grep -q "$version"; then
  echo "wasm-bindgen $version is needed: cargo install wasm-bindgen-cli --version $version" >&2
  exit 1
fi
wasm-bindgen --target web --no-typescript --remove-name-section --remove-producers-section --out-dir pkg \
  "$target/wasm32-unknown-unknown/wasm-release/nuage_site.wasm"
echo "browser bundle ready in pkg/ ($(du -h pkg/nuage_site_bg.wasm | cut -f1))"
