//! The user's RunPod API key. It lives in the OS keychain and is held in
//! memory as a [`SecretString`]; it is never logged, printed or written to a
//! file, and its `Debug` output is redacted.

use std::fmt;
use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};

/// The keychain service and account the RunPod key is stored under.
pub const KEY_SERVICE: &str = "fonsviva";
pub const KEY_ACCOUNT: &str = "runpod";

#[derive(Clone)]
pub struct ApiKey(Arc<SecretString>);

impl ApiKey {
    pub fn new(secret: SecretString) -> Self {
        Self(Arc::new(secret))
    }

    /// Reads the key from the OS keychain; `Ok(None)` when none is stored.
    pub fn from_keychain() -> Result<Option<Self>, String> {
        let entry =
            keyring::Entry::new(KEY_SERVICE, KEY_ACCOUNT).map_err(|error| error.to_string())?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(Self::new(SecretString::from(secret)))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Stores `secret` in the OS keychain, replacing any earlier key.
    pub fn store_in_keychain(secret: &SecretString) -> Result<(), String> {
        let entry =
            keyring::Entry::new(KEY_SERVICE, KEY_ACCOUNT).map_err(|error| error.to_string())?;
        entry
            .set_password(secret.expose_secret())
            .map_err(|error| error.to_string())
    }

    pub fn delete_from_keychain() -> Result<(), String> {
        let entry =
            keyring::Entry::new(KEY_SERVICE, KEY_ACCOUNT).map_err(|error| error.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_key() {
        let key = ApiKey::new(SecretString::from("rpa_live_SECRET"));
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert!(!format!("{:?}", Some(key)).contains("SECRET"));
    }
}
