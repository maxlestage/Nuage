//! The files of the site (`static/`) and the browser bundle (`pkg/`), embedded in the binary
//! by `build.rs` and served from memory. Text files are compressed once (brotli and gzip),
//! the first time they are asked for.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::OnceLock;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

pub struct Asset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    /// Short content hash, used in `?v=` so that the file can be cached forever.
    pub version: String,
    brotli: OnceLock<Option<Vec<u8>>>,
    gzip: OnceLock<Option<Vec<u8>>>,
}

pub struct Assets {
    files: BTreeMap<&'static str, Asset>,
}

pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// FNV-1a, enough to tell versions apart.
pub fn fnv(seed: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(seed, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

pub fn short_hash(bytes: &[u8]) -> String {
    format!("{:016x}", fnv(0xcbf2_9ce4_8422_2325, bytes))[..10].to_string()
}

/// Compresses `bytes` with brotli (`true`) or gzip, if that makes them smaller.
pub fn compress(bytes: &[u8], brotli: bool, best: bool) -> Option<Vec<u8>> {
    let out = if brotli {
        let quality = if best { 11 } else { 5 };
        let params = brotli::enc::BrotliEncoderParams { quality, lgwin: 22, ..Default::default() };
        let mut out = Vec::new();
        brotli::enc::BrotliCompress(&mut &bytes[..], &mut out, &params).ok()?;
        out
    } else {
        let level = if best { flate2::Compression::best() } else { flate2::Compression::default() };
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), level);
        encoder.write_all(bytes).ok()?;
        encoder.finish().ok()?
    };
    (out.len() < bytes.len()).then_some(out)
}

impl Asset {
    fn new(bytes: &'static [u8], content_type: &'static str) -> Asset {
        Asset { version: short_hash(bytes), bytes, content_type, brotli: OnceLock::new(), gzip: OnceLock::new() }
    }

    /// Text, styles, scripts, WebAssembly, SVG: worth compressing.
    fn compressible(&self) -> bool {
        self.bytes.len() >= 512
            && (self.content_type.starts_with("text/")
                || self.content_type.starts_with("application/")
                || self.content_type == "image/svg+xml")
    }

    pub fn brotli(&self) -> Option<&[u8]> {
        self.brotli.get_or_init(|| if self.compressible() { compress(self.bytes, true, true) } else { None }).as_deref()
    }

    pub fn gzip(&self) -> Option<&[u8]> {
        self.gzip.get_or_init(|| if self.compressible() { compress(self.bytes, false, true) } else { None }).as_deref()
    }
}

impl Assets {
    /// The embedded files.
    pub fn embedded() -> Assets {
        let files = embedded::FILES.iter().map(|(url, bytes)| (*url, Asset::new(bytes, content_type(url)))).collect();
        Assets { files }
    }

    pub fn get(&self, url: &str) -> Option<&Asset> {
        self.files.get(url)
    }

    /// `url?v=<hash>` for files that exist, so that they can be cached forever.
    pub fn url(&self, url: &str) -> String {
        match self.files.get(url) {
            Some(asset) => format!("{url}?v={}", asset.version),
            None => url.to_string(),
        }
    }

    /// Every file, with its URL, sorted by URL.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &Asset)> {
        self.files.iter().map(|(url, asset)| (*url, asset))
    }

    /// Whether the browser bundle (`pkg/`) was embedded.
    pub fn has_bundle(&self) -> bool {
        self.files.contains_key("/pkg/nuage_site.js") && self.files.contains_key("/pkg/nuage_site_bg.wasm")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_text_but_not_images() {
        let css: &'static [u8] = "body { color: red; }\n".repeat(100).leak().as_bytes();
        let css = Asset::new(css, content_type("app.css"));
        assert!(css.brotli().unwrap().len() < css.bytes.len());
        assert!(css.gzip().unwrap().len() < css.bytes.len());
        let png = Asset::new(&[7; 4096], content_type("icon.png"));
        assert!(png.brotli().is_none() && png.gzip().is_none());
        assert_eq!(content_type("manifest.webmanifest"), "application/manifest+json");
    }

    #[test]
    fn the_site_files_are_embedded() {
        let assets = Assets::embedded();
        for url in ["/static/app.css", "/static/favicon.svg", "/static/og-image.png", "/static/icon-512.png"] {
            assert!(assets.get(url).is_some(), "{url}");
        }
        assert!(assets.url("/static/app.css").starts_with("/static/app.css?v="));
    }
}
