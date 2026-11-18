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

fn read_outcome(result: Result<String, Error>, account: &str) -> Result<Option<String>, String> {
    match result {
        Ok(secret) => Ok(Some(secret)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => Err(failure("read", account, &e)),
    }
}

fn write_outcome(result: Result<(), Error>, account: &str) -> Result<(), String> {
    result.map_err(|e| failure("write", account, &e))
}

fn delete_outcome(result: Result<(), Error>, account: &str) -> Result<bool, String> {
    match result {
        Ok(()) => Ok(true),
        Err(Error::NoEntry) => Ok(false),
        Err(e) => Err(failure("delete", account, &e)),
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        read_outcome(Self::entry(account)?.get_password(), account)
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        write_outcome(Self::entry(account)?.set_password(secret), account)
    }

    fn delete(&self, account: &str) -> Result<bool, String> {
        delete_outcome(Self::entry(account)?.delete_credential(), account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_failure() -> Error {
        Error::PlatformFailure("keychain locked".into())
    }

    #[test]
    fn a_stored_secret_is_read_back() {
        let out = read_outcome(Ok("s3cret".to_string()), "github");

        assert_eq!(out, Ok(Some("s3cret".to_string())));
    }

    #[test]
    fn a_missing_entry_reads_as_none() {
        let out = read_outcome(Err(Error::NoEntry), "github");

        assert_eq!(out, Ok(None));
    }

    #[test]
    fn a_read_failure_names_the_action_service_and_account() {
        let err = read_outcome(Err(platform_failure()), "github").unwrap_err();

        assert_eq!(
            err,
            format!(
                "the OS credential store failed to read the entry for service {CREDENTIAL_SERVICE} account github: Platform failure: keychain locked"
            )
        );
    }

    #[test]
    fn a_successful_write_is_ok() {
        assert_eq!(write_outcome(Ok(()), "github"), Ok(()));
    }

    #[test]
    fn a_write_failure_names_the_write_action() {
        let err = write_outcome(Err(platform_failure()), "github").unwrap_err();

        assert!(err.contains("failed to write the entry"), "{err}");
        assert!(err.contains("account github"), "{err}");
        assert!(err.ends_with("Platform failure: keychain locked"), "{err}");
    }

    #[test]
    fn deleting_an_existing_entry_reports_it_existed() {
        assert_eq!(delete_outcome(Ok(()), "github"), Ok(true));
    }

    #[test]
    fn deleting_a_missing_entry_reports_it_did_not_exist() {
        assert_eq!(delete_outcome(Err(Error::NoEntry), "github"), Ok(false));
    }

    #[test]
    fn a_delete_failure_names_the_delete_action() {
        let err = delete_outcome(Err(platform_failure()), "github").unwrap_err();

        assert!(err.contains("failed to delete the entry"), "{err}");
        assert!(err.contains("account github"), "{err}");
    }

    #[test]
    fn an_open_failure_names_the_open_action() {
        let err = failure("open", "github", &Error::NoDefaultStore);

        assert!(err.contains("failed to open the entry"), "{err}");
        assert!(err.contains(&format!("service {CREDENTIAL_SERVICE} account github")));
    }

    /// Runs only where the Secret Service is unreachable (a headless CI
    /// runner), so it never reads or writes a real credential store.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unreachable_os_store_fails_every_operation_with_an_open_error() {
        if Entry::store_status().is_ok() {
            return;
        }
        let store = KeyringCredentialStore;

        let errors = [
            store.get("llm-hub-test").unwrap_err(),
            store.set("llm-hub-test", "secret").unwrap_err(),
            store.delete("llm-hub-test").unwrap_err(),
        ];

        for err in errors {
            assert!(err.contains("failed to open the entry"), "{err}");
            assert!(err.contains("account llm-hub-test"), "{err}");
        }
    }
}
