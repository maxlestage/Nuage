//! What the editor remembers between two launches, on this computer only.
//!
//! The language, the theme and the addresses of the servers are kept by
//! eframe with the window's state. A management token is kept only when its
//! owner asks for it, in a file of its own that only they can read (on
//! Linux and macOS; on Windows, their profile folder is theirs already).

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The tokens remembered, by server address.
#[derive(Clone, Debug, Default)]
pub struct Tokens {
    file: Option<PathBuf>,
    tokens: BTreeMap<String, String>,
}

impl Tokens {
    /// The tokens kept in `dir` (`None`: nothing is kept, as in the tests).
    pub fn open(dir: Option<PathBuf>) -> Tokens {
        let file = dir.map(|d| d.join("tokens.json"));
        let tokens = file
            .as_ref()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Tokens { file, tokens }
    }

    /// The token remembered for `address`.
    pub fn get(&self, address: &str) -> Option<&str> {
        self.tokens.get(address).map(String::as_str)
    }

    /// Remembers (`Some`) or forgets (`None`) the token of `address`.
    pub fn set(&mut self, address: &str, token: Option<&str>) -> std::io::Result<()> {
        if self.get(address) == token {
            return Ok(());
        }
        match token {
            Some(t) => self.tokens.insert(address.to_owned(), t.to_owned()),
            None => self.tokens.remove(address),
        };
        self.write()
    }

    fn write(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else { return Ok(()) };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_vec_pretty(&self.tokens).unwrap_or_default();
        // Written aside, readable by its owner only, then put in place.
        let tmp = file.with_extension("partial");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        {
            use std::io::Write;
            let mut f = options.open(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_kept_for_their_owner_only() {
        let dir = std::env::temp_dir().join(format!("nuage-editor-tokens-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut tokens = Tokens::open(Some(dir.clone()));
        tokens.set("http://127.0.0.1:8080", Some("secret")).unwrap();
        let again = Tokens::open(Some(dir.clone()));
        assert_eq!(again.get("http://127.0.0.1:8080"), Some("secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("tokens.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        tokens.set("http://127.0.0.1:8080", None).unwrap();
        assert_eq!(Tokens::open(Some(dir.clone())).get("http://127.0.0.1:8080"), None);
        // Nowhere to keep them: nothing is written, nothing fails.
        Tokens::open(None).set("x", Some("y")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
