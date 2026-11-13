//! Tokens of SQL.

use memoire_orm::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok {
    /// A name or a keyword, as written.
    Word(String),
    /// `"name"`, `` `name` `` or `[name]`: always a name.
    Quoted(String),
    /// `'text'`.
    Str(String),
    /// `12`, `-` is an operator.
    Int(String),
    /// `1.5`, `2e3`.
    Float(String),
    /// `?` (the next one), `?3` or `$3` (the third).
    Param(Option<usize>),
    Sym(&'static str),
}

pub(crate) fn syntax(msg: impl Into<String>) -> Error {
    Error::Validation(format!("SQL: {}", msg.into()))
}

const SYMS: [&str; 22] = [
    "<=", ">=", "<>", "!=", "==", "||", "::", "(", ")", ",", ".", ";", "*", "+", "-", "/", "%", "=", "<", ">", "&", "|",
];

/// The tokens of `src`.
pub(crate) fn tokenize(src: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) {
            let start = i;
            let mut float = false;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i < chars.len() && chars[i] == '.' && chars.get(i + 1).is_none_or(|n| *n != '.') {
                float = true;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < chars.len() && (chars[i] == 'e' || chars[i] == 'E') {
                let mut j = i + 1;
                if j < chars.len() && (chars[j] == '+' || chars[j] == '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    float = true;
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            out.push(if float { Tok::Float(text) } else { Tok::Int(text) });
        } else if c == '\'' || c == '"' || c == '`' || c == '[' {
            let close = if c == '[' { ']' } else { c };
            let mut s = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err(syntax("a quote is not closed")),
                    Some(&q) if q == close && close != ']' && chars.get(i + 1) == Some(&close) => {
                        s.push(close);
                        i += 2;
                    }
                    Some(&q) if q == close => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(if c == '\'' { Tok::Str(s) } else { Tok::Quoted(s) });
        } else if c == '?' || (c == '$' && next.is_some_and(|n| n.is_ascii_digit())) {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let n: Option<usize> = if i > start {
                let n: usize =
                    chars[start..i].iter().collect::<String>().parse().map_err(|_| syntax("bad parameter"))?;
                if n == 0 {
                    return Err(syntax("parameters are numbered from 1"));
                }
                Some(n - 1)
            } else if c == '$' {
                return Err(syntax("`$` must be followed by a number"));
            } else {
                None
            };
            out.push(Tok::Param(n));
        } else {
            let two: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let sym = SYMS
                .iter()
                .find(|s| s.len() == 2 && **s == two)
                .or_else(|| SYMS.iter().find(|s| s.len() == 1 && s.starts_with(c)))
                .ok_or_else(|| syntax(format!("unexpected `{c}`")))?;
            i += sym.len();
            out.push(Tok::Sym(sym));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let t = tokenize("SELECT a.\"b c\", 'it''s', 1.5e3, ?2, $1, ? -- note\n FROM t /* x */ WHERE x <> -3 || y")
            .unwrap();
        assert_eq!(t[1], Tok::Word("a".into()));
        assert_eq!(t[3], Tok::Quoted("b c".into()));
        assert_eq!(t[5], Tok::Str("it's".into()));
        assert_eq!(t[7], Tok::Float("1.5e3".into()));
        assert_eq!(
            (t[9].clone(), t[11].clone(), t[13].clone()),
            (Tok::Param(Some(1)), Tok::Param(Some(0)), Tok::Param(None))
        );
        assert!(t.contains(&Tok::Sym("<>")) && t.contains(&Tok::Sym("||")));
        assert!(tokenize("'open").is_err());
        assert!(tokenize("select #").is_err());
    }
}
