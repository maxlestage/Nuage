//! Where a management token is kept between two launches, when its owner
//! asks for it: the system's password store only (the macOS Keychain, the
//! Windows Credential Manager, the Secret Service of Linux desktops), never
//! a file of the editor. Every call may wait for the person (an Allow
//! dialog, a keyring to unlock): the window runs them on a thread of their
//! own (see [`crate::jobs`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::i18n::{Lang, t};
use crate::secret::Secret;

/// The name the tokens are kept under in the system's store (the address
/// of the server is the account).
pub const SERVICE: &str = crate::APP_ID;

/// A password store.
pub trait Vault: Send + Sync {
    /// Which store it is, for the sentences that say where a token goes.
    fn store(&self) -> Store;
    /// `Ok` when tokens can be kept; else why not, in the system's words.
    fn status(&self) -> Result<(), String>;
    /// The token kept for `address` (`None`: none).
    fn get(&self, address: &str) -> Result<Option<Secret>, String>;
    fn set(&self, address: &str, token: &Secret) -> Result<(), String>;
    /// Forgets the token of `address` (none kept is no error).
    fn delete(&self, address: &str) -> Result<(), String>;
}

/// The kinds of stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    Keychain,
    CredentialManager,
    SecretService,
    /// The tests' store, in memory.
    Memory,
}

impl Store {
    /// The store of this system.
    pub fn of_system() -> Store {
        if cfg!(target_os = "macos") {
            Store::Keychain
        } else if cfg!(windows) {
            Store::CredentialManager
        } else {
            Store::SecretService
        }
    }

    /// Where it is, as a sentence names it ("Token kept in …").
    pub fn place(self, l: Lang) -> &'static str {
        match self {
            Store::Keychain => t(l, "your login Keychain"),
            Store::CredentialManager => t(l, "Windows Credential Manager"),
            Store::SecretService => t(l, "your desktop's keyring (Secret Service)"),
            Store::Memory => t(l, "the editor's memory, until it closes"),
        }
    }
}

/// The system's store, through the keyring crate.
pub struct KeyringVault;

impl KeyringVault {
    fn entry(address: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, address).map_err(|e| e.to_string())
    }
}

impl Vault for KeyringVault {
    fn store(&self) -> Store {
        Store::of_system()
    }

    fn status(&self) -> Result<(), String> {
        keyring::Entry::store_status().as_ref().map(|_| ()).map_err(|e| e.to_string())
    }

    fn get(&self, address: &str) -> Result<Option<Secret>, String> {
        match Self::entry(address)?.get_password() {
            Ok(token) => Ok(Some(Secret::new(token))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, address: &str, token: &Secret) -> Result<(), String> {
        Self::entry(address)?.set_password(token.expose()).map_err(|e| e.to_string())
    }

    fn delete(&self, address: &str) -> Result<(), String> {
        match Self::entry(address)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// A store in memory, for the tests: what the system's would do, without it.
#[derive(Default)]
pub struct MemoryVault {
    tokens: Mutex<BTreeMap<String, Secret>>,
}

impl MemoryVault {
    /// The addresses it holds a token for.
    pub fn addresses(&self) -> Vec<String> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect()
    }
}

impl Vault for MemoryVault {
    fn store(&self) -> Store {
        Store::Memory
    }

    fn status(&self) -> Result<(), String> {
        Ok(())
    }

    fn get(&self, address: &str) -> Result<Option<Secret>, String> {
        let tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(tokens.get(address).map(|s| Secret::new(s.expose().to_owned())))
    }

    fn set(&self, address: &str, token: &Secret) -> Result<(), String> {
        let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        tokens.insert(address.to_owned(), Secret::new(token.expose().to_owned()));
        Ok(())
    }

    fn delete(&self, address: &str) -> Result<(), String> {
        self.tokens.lock().unwrap_or_else(PoisonError::into_inner).remove(address);
        Ok(())
    }
}

/// No store this editor can use (none on this system, or it refused): the
/// reason is said, and nothing is kept.
pub struct Unavailable(pub String);

impl Vault for Unavailable {
    fn store(&self) -> Store {
        Store::of_system()
    }

    fn status(&self) -> Result<(), String> {
        Err(self.0.clone())
    }

    fn get(&self, _address: &str) -> Result<Option<Secret>, String> {
        Err(self.0.clone())
    }

    fn set(&self, _address: &str, _token: &Secret) -> Result<(), String> {
        Err(self.0.clone())
    }

    fn delete(&self, _address: &str) -> Result<(), String> {
        Err(self.0.clone())
    }
}

/// Deletes the `tokens.json` an early build of the editor kept tokens in,
/// in clear, in its folder `dir`. `true` when there was one.
pub fn delete_old_tokens_file(dir: &Path) -> Result<bool, String> {
    let mut found = false;
    for name in ["tokens.json", "tokens.partial"] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => found = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", dir.join(name).display())),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_in_memory() {
        let vault = MemoryVault::default();
        assert!(vault.status().is_ok());
        assert_eq!(vault.get("http://127.0.0.1:8080").unwrap(), None);
        vault.set("http://127.0.0.1:8080", &Secret::new("0123456789abcdef0123456789abcdef".into())).unwrap();
        assert_eq!(vault.get("http://127.0.0.1:8080").unwrap().unwrap().expose(), "0123456789abcdef0123456789abcdef");
        vault.delete("http://127.0.0.1:8080").unwrap();
        vault.delete("http://127.0.0.1:8080").unwrap();
        assert!(vault.addresses().is_empty());
        let none = Unavailable("no Secret Service on this session bus".into());
        assert_eq!(none.status(), Err("no Secret Service on this session bus".into()));
        assert!(none.set("x", &Secret::new("y".into())).is_err());
        for l in Lang::ALL {
            for store in [Store::Keychain, Store::CredentialManager, Store::SecretService, Store::Memory] {
                assert!(!store.place(l).is_empty());
            }
        }
    }

    #[test]
    fn the_old_tokens_file_goes() {
        let dir = std::env::temp_dir().join(format!("nuage-editor-old-tokens-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(delete_old_tokens_file(&dir), Ok(false));
        std::fs::write(dir.join("tokens.json"), r#"{"http://127.0.0.1:8080":"made-up"}"#).unwrap();
        assert_eq!(delete_old_tokens_file(&dir), Ok(true));
        assert!(!dir.join("tokens.json").exists());
        assert_eq!(delete_old_tokens_file(&dir), Ok(false), "said once");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
