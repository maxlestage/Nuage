//! Embeds every file of `static/` and of the browser bundle `pkg/` (built by
//! `build-wasm.sh`) in the server: the Nuage DB binary serves the whole site without any
//! file next to it. Writes `$OUT_DIR/assets.rs`, a list of `(url, bytes)`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        // Hidden files, and what wasm-bindgen writes besides the bundle.
        if name.starts_with('.') || name == "package.json" || name.ends_with(".d.ts") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect(&path, &format!("{prefix}/{name}"), out);
        } else {
            out.push((format!("{prefix}/{name}"), path));
        }
    }
}

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=static");
    println!("cargo:rerun-if-changed=pkg");
    println!("cargo:rerun-if-changed=build.rs");
    let mut files = Vec::new();
    // The browser bundle itself does not embed the files: only the server does.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("wasm32") {
        collect(&root.join("static"), "/static", &mut files);
        collect(&root.join("pkg"), "/pkg", &mut files);
        if !files.iter().any(|(url, _)| url == "/pkg/nuage_site_bg.wasm") {
            println!("cargo:warning=nuage-site: pkg/ has no browser bundle, /app will not be live (run build-wasm.sh)");
        }
    }
    let mut code = String::from("/// Every embedded file: (URL, contents).\npub static FILES: &[(&str, &[u8])] = &[\n");
    for (url, path) in files {
        let _ = writeln!(code, "    ({url:?}, include_bytes!({:?})),", path.display().to_string());
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("assets.rs");
    std::fs::write(out, code).expect("write assets.rs");
}
