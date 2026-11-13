//! A small syntax highlighter for the code samples (Rust and shell): keywords, strings,
//! comments, macros, types, numbers. The colours come from the stylesheet (`.tk-*`).

fn escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

fn span(out: &mut String, class: &str, text: &str) {
    out.push_str("<span class=\"tk-");
    out.push_str(class);
    out.push_str("\">");
    escape(out, text);
    out.push_str("</span>");
}

const RUST_KEYWORDS: [&str; 18] = [
    "let", "mut", "fn", "use", "pub", "struct", "async", "await", "impl", "match", "if", "else", "return", "for", "in",
    "mod", "const", "self",
];

/// `code` as HTML, highlighted for `lang` (`rust` or `sh`).
pub fn highlight(code: &str, lang: &str) -> String {
    let mut out = String::with_capacity(code.len() * 2);
    let comment = if lang == "rust" { "//" } else { "#" };
    for (n, line) in code.lines().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let mut rest = line;
        while !rest.is_empty() {
            if rest.starts_with(comment) {
                span(&mut out, "com", rest);
                break;
            }
            let c = rest.chars().next().unwrap_or(' ');
            if c == '"' {
                let end = rest[1..].find('"').map_or(rest.len(), |e| e + 2);
                span(&mut out, "str", &rest[..end]);
                rest = &rest[end..];
            } else if c.is_ascii_digit() {
                let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '.').unwrap_or(rest.len());
                span(&mut out, "num", &rest[..end]);
                rest = &rest[end..];
            } else if c.is_alphabetic() || c == '_' || c == '$' {
                let end = rest[1..].find(|c: char| !c.is_alphanumeric() && c != '_').map_or(rest.len(), |e| e + 1);
                let word = &rest[..end];
                let after = &rest[end..];
                let class = if lang != "rust" {
                    let shouting = word.len() > 1 && word.chars().all(|c| c.is_ascii_uppercase() || c == '_');
                    (word.starts_with('$') || shouting).then_some("var")
                } else if RUST_KEYWORDS.contains(&word) {
                    Some("kw")
                } else if after.starts_with('!') {
                    Some("mac")
                } else if word.chars().all(|c| c.is_ascii_uppercase() || c == '_') && word.len() > 1 {
                    Some("const")
                } else if word.starts_with(|c: char| c.is_ascii_uppercase()) {
                    Some("ty")
                } else if after.starts_with('(') || after.starts_with("::<") {
                    Some("fn")
                } else {
                    None
                };
                match class {
                    Some(class) => span(&mut out, class, word),
                    None => escape(&mut out, word),
                }
                rest = after;
            } else {
                let mut buf = [0; 4];
                escape(&mut out, c.encode_utf8(&mut buf));
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_rust_and_shell() {
        let html = highlight("let db = NuageDb::open(\"data/\")?; // embedded", "rust");
        assert!(
            html.starts_with(r#"<span class="tk-kw">let</span> db = <span class="tk-ty">NuageDb</span>"#),
            "{html}"
        );
        assert!(html.contains(r#"<span class="tk-str">&quot;data/&quot;</span>"#));
        assert!(html.contains(r#"<span class="tk-com">// embedded</span>"#));
        let sh = highlight("# start\nNUAGE_TOKEN=x nuage", "sh");
        assert!(
            sh.contains(r#"<span class="tk-com"># start</span>"#)
                && sh.contains(r#"<span class="tk-var">NUAGE_TOKEN</span>"#),
            "{sh}"
        );
    }
}
