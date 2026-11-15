//! A secret (a management token, an application's token) as the editor
//! holds it: never printed, never copied by accident, wiped from memory
//! when dropped.

use zeroize::Zeroizing;

/// A secret text. `Debug` prints `Secret(***)`; there is no `Display`,
/// `Clone` or `Serialize`: its value is read only through [`Secret::expose`].
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Takes `text` as a secret (its buffer is wiped when the secret is dropped).
    pub fn new(text: String) -> Secret {
        Secret(Zeroizing::new(text))
    }

    /// The value, for the one place that sends or shows it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Its number of characters.
    pub fn chars(&self) -> usize {
        self.0.chars().count()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl PartialEq for Secret {
    fn eq(&self, other: &Secret) -> bool {
        // Only the tests compare secrets; the server compares in constant time.
        self.expose() == other.expose()
    }
}

impl Eq for Secret {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_printed() {
        let s = Secret::new("hunter2-0123456789abcdef0123456789".into());
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(format!("{:?}", Some(&s)), "Some(Secret(***))");
        assert_eq!(s.expose(), "hunter2-0123456789abcdef0123456789");
        assert_eq!(s.chars(), 34);
    }
}
