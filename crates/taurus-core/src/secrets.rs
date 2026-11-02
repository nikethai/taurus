//! Where the passwords of protected downloads are kept: the OS keychain
//! (service [`SERVICE`], one entry per job), or memory when there is none.
//! Never the database, the settings, events, views or logs.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::errors::{CoreError, ErrorKind};

/// The keychain service every password is filed under.
pub const SERVICE: &str = "dev.nikethai.taurus";

/// A store of secrets by account name (`job:<id>` for a job's password).
pub trait SecretStore: Send + Sync {
    fn set(&self, account: &str, secret: &str) -> Result<(), CoreError>;
    /// `None` when the account has no secret.
    fn get(&self, account: &str) -> Result<Option<String>, CoreError>;
    /// Deleting an account without a secret is not an error.
    fn delete(&self, account: &str) -> Result<(), CoreError>;
}

/// The OS keychain: Keychain Services on macOS, Credential Manager on
/// Windows, the Secret Service (over D-Bus) elsewhere. Every call fails
/// when the platform has no usable keychain (e.g. a Linux session without a
/// Secret Service); the engine then keeps passwords in memory.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(account: &str) -> Result<keyring::Entry, CoreError> {
        keyring::Entry::new(SERVICE, account).map_err(keychain_error)
    }
}

impl SecretStore for KeyringStore {
    fn set(&self, account: &str, secret: &str) -> Result<(), CoreError> {
        Self::entry(account)?
            .set_password(secret)
            .map_err(keychain_error)
    }

    fn get(&self, account: &str) -> Result<Option<String>, CoreError> {
        match Self::entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn delete(&self, account: &str) -> Result<(), CoreError> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keychain_error(e)),
        }
    }
}

/// The keychain's error. Its text never holds a secret (`keyring`'s
/// messages name no secret bytes).
fn keychain_error(e: keyring::Error) -> CoreError {
    CoreError {
        kind: ErrorKind::Unknown,
        detail: format!("keychain: {e}"),
    }
}

/// Secrets in memory, gone when the process ends. For tests, and the
/// engine's fallback when there is no keychain.
#[derive(Default)]
pub struct MemoryStore {
    secrets: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        // Nothing panics while holding the lock, so the map is still whole.
        self.secrets.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStore")
            .field("accounts", &self.lock().len())
            .finish()
    }
}

impl SecretStore for MemoryStore {
    fn set(&self, account: &str, secret: &str) -> Result<(), CoreError> {
        self.lock().insert(account.to_owned(), secret.to_owned());
        Ok(())
    }

    fn get(&self, account: &str) -> Result<Option<String>, CoreError> {
        Ok(self.lock().get(account).cloned())
    }

    fn delete(&self, account: &str) -> Result<(), CoreError> {
        self.lock().remove(account);
        Ok(())
    }
}

/// A user name and password for a protected (HTTP Basic auth) download.
/// Not serializable, and its `Debug` hides the password.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &format_args!("***"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryStore::new();
        assert_eq!(store.get("job:1").unwrap(), None);
        store.set("job:1", "one").unwrap();
        store.set("job:2", "two").unwrap();
        assert_eq!(store.get("job:1").unwrap().as_deref(), Some("one"));
        store.set("job:1", "uno").unwrap();
        assert_eq!(store.get("job:1").unwrap().as_deref(), Some("uno"));
        store.delete("job:1").unwrap();
        assert_eq!(store.get("job:1").unwrap(), None);
        // Deleting what is not there is fine; other accounts are untouched.
        store.delete("job:1").unwrap();
        assert_eq!(store.get("job:2").unwrap().as_deref(), Some("two"));
        assert!(!format!("{store:?}").contains("two"));
    }

    #[test]
    fn debug_hides_password() {
        let creds = Credentials {
            username: "ann".into(),
            password: "s3cr3t-Pa55".into(),
        };
        let shown = format!("{creds:?}");
        assert_eq!(shown, "Credentials { username: \"ann\", password: *** }");
        assert!(!format!("{creds:#?}").contains("s3cr3t"));
        let request = crate::AddRequest {
            credentials: Some(creds),
            ..crate::AddRequest::default()
        };
        assert!(!format!("{request:?}").contains("s3cr3t"));
    }
}
