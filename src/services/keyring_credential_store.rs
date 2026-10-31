use keyring::{Entry, Error};

use crate::consts::CREDENTIAL_SERVICE;
use crate::services::credential_store::CredentialStore;

/// The OS credential store: macOS Keychain, Windows Credential Manager, or
/// the Secret Service on other Unix systems.
#[derive(Debug, Default)]
pub struct KeyringCredentialStore;

impl KeyringCredentialStore {
    fn entry(account: &str) -> Result<Entry, String> {
        Entry::new(CREDENTIAL_SERVICE, account).map_err(|e| failure("open", account, &e))
    }
}

fn failure(action: &str, account: &str, error: &Error) -> String {
    format!(
        "the OS credential store failed to {action} the entry for service {CREDENTIAL_SERVICE} account {account}: {error}"
    )
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        match Self::entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(failure("read", account, &e)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        Self::entry(account)?
            .set_password(secret)
            .map_err(|e| failure("write", account, &e))
    }

    fn delete(&self, account: &str) -> Result<bool, String> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(Error::NoEntry) => Ok(false),
            Err(e) => Err(failure("delete", account, &e)),
        }
    }
}
