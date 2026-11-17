//! API keys live in the OS keychain. Config files get a reference: a
//! command the harness runs at request time to read the key (pi and Eukhe
//! both run a leading `!command`), never the key itself.

use std::collections::BTreeMap;
use std::sync::Mutex;

use secrecy::{ExposeSecret, SecretString};

use crate::Os;

/// The keychain service every Fonsviva wiring key is stored under.
pub const SERVICE: &str = "fonsviva";

/// Where keys are kept. The daemon uses [`OsKeychain`]; tests and the demo
/// mode use [`MemoryKeys`] so they never touch the real keychain.
pub trait KeyStore: Send + Sync {
    fn set(&self, account: &str, secret: &SecretString) -> Result<(), String>;
    fn get(&self, account: &str) -> Result<Option<SecretString>, String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The OS keychain: Keychain Services on macOS, Credential Manager on
/// Windows, the Secret Service elsewhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeychain;

impl KeyStore for OsKeychain {
    fn set(&self, account: &str, secret: &SecretString) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|error| error.to_string())?;
        entry
            .set_password(secret.expose_secret())
            .map_err(|error| error.to_string())
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|error| error.to_string())?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(SecretString::from(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|error| error.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// An in-memory store for tests and demo mode.
#[derive(Default)]
pub struct MemoryKeys(Mutex<BTreeMap<String, SecretString>>);

impl KeyStore for MemoryKeys {
    fn set(&self, account: &str, secret: &SecretString) -> Result<(), String> {
        let mut map = self
            .0
            .lock()
            .map_err(|_| "key store lock poisoned".to_string())?;
        map.insert(
            account.to_string(),
            SecretString::from(secret.expose_secret().to_string()),
        );
        Ok(())
    }

    fn get(&self, account: &str) -> Result<Option<SecretString>, String> {
        let map = self
            .0
            .lock()
            .map_err(|_| "key store lock poisoned".to_string())?;
        Ok(map
            .get(account)
            .map(|secret| SecretString::from(secret.expose_secret().to_string())))
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        let mut map = self
            .0
            .lock()
            .map_err(|_| "key store lock poisoned".to_string())?;
        map.remove(account);
        Ok(())
    }
}

/// The command that prints the key for `account` to stdout. macOS and Linux
/// use the OS's own keychain tool. Windows has no built-in reader for
/// Credential Manager, so it calls the `fonsviva` CLI, which must provide
/// `fonsviva key print <account>` (it calls [`crate::reveal_key`]).
pub fn read_command(os: Os, account: &str) -> String {
    match os {
        Os::MacOs => format!("security find-generic-password -s {SERVICE} -a {account} -w"),
        Os::Linux => format!("secret-tool lookup service {SERVICE} username {account}"),
        Os::Windows => format!("fonsviva key print {account}"),
    }
}
