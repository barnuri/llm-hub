use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::services::credential_store::CredentialStore;

/// In-process stand-in for the OS credential store, so tests never touch the
/// real keychain.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    secrets: Mutex<HashMap<String, String>>,
}

impl MemoryCredentialStore {
    fn secrets(&self) -> Result<MutexGuard<'_, HashMap<String, String>>, String> {
        self.secrets
            .lock()
            .map_err(|_| "the memory credential store lock is poisoned".to_string())
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        Ok(self.secrets()?.get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        self.secrets()?
            .insert(account.to_string(), secret.to_string());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<bool, String> {
        Ok(self.secrets()?.remove(account).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "github-copilot:github.com";

    #[test]
    fn set_then_get_returns_secret() {
        let store = MemoryCredentialStore::default();

        store.set(ACCOUNT, "first").unwrap();
        store.set(ACCOUNT, "second").unwrap();

        assert_eq!(store.get(ACCOUNT).unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn missing_account_returns_none() {
        let store = MemoryCredentialStore::default();

        assert_eq!(store.get(ACCOUNT).unwrap(), None);
    }

    #[test]
    fn delete_reports_whether_entry_existed() {
        let store = MemoryCredentialStore::default();
        store.set(ACCOUNT, "secret").unwrap();

        assert!(store.delete(ACCOUNT).unwrap());
        assert!(!store.delete(ACCOUNT).unwrap());
        assert_eq!(store.get(ACCOUNT).unwrap(), None);
    }
}
