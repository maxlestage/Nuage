//! Security headers, including a Content-Security-Policy that allows WebAssembly and pins
//! the inline scripts of each page (the WebAssembly loader and the service worker
//! registration written by active) by their SHA-256 hash.

use base64::Engine;
use http::{HeaderMap, HeaderName, HeaderValue, header};
use sha2::{Digest, Sha256};

/// Bodies of the executable inline `<script>` elements of `html` (no `src`, not JSON-LD).
pub fn inline_scripts(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let after = &rest[start..];
        let Some(tag_end) = after.find('>') else { break };
        let tag = &after[..tag_end];
        let body = &after[tag_end + 1..];
        let Some(close) = body.find("</script>") else { break };
        if !tag.contains(" src=") && !tag.contains("application/ld+json") {
            out.push(&body[..close]);
        }
        rest = &body[close + "</script>".len()..];
    }
    out
}

/// The Content-Security-Policy of a page.
pub fn csp(html: &str) -> String {
    let hashes: Vec<String> = inline_scripts(html)
        .into_iter()
        .map(|body| {
            let digest = Sha256::digest(body.as_bytes());
            format!("'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(digest))
        })
        .collect();
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval' {}; style-src 'self' 'unsafe-inline'; \
         font-src 'self'; img-src 'self' data:; connect-src 'self'; worker-src 'self'; manifest-src 'self'; \
         object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'",
        hashes.join(" ")
    )
}

/// The headers sent with every response (pages add their [`csp`]).
pub fn headers(headers: &mut HeaderMap, https: bool) {
    let fixed: [(HeaderName, &'static str); 5] = [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "strict-origin-when-cross-origin"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (HeaderName::from_static("permissions-policy"), "camera=(), microphone=(), geolocation=(), payment=()"),
        (HeaderName::from_static("cross-origin-opener-policy"), "same-origin"),
    ];
    for (name, value) in fixed {
        headers.insert(name, HeaderValue::from_static(value));
    }
    if https {
        headers
            .insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000; includeSubDomains"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_executable_inline_scripts_only() {
        let html = concat!(
            r#"<head><script>a()</script><script src="/x.js"></script>"#,
            r#"<script type="module">b()</script><script type="application/ld+json">{}</script></head>"#
        );
        assert_eq!(inline_scripts(html), vec!["a()", "b()"]);
        let csp = csp(html);
        assert!(csp.contains("'wasm-unsafe-eval'"));
        assert_eq!(csp.matches("'sha256-").count(), 2);
        assert!(csp.contains("worker-src 'self'") && csp.contains("manifest-src 'self'"));
    }
}
