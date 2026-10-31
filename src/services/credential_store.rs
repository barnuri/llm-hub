/// Secret storage keyed by account name. Backends may block on OS services,
/// so async callers run each call through `spawn_blocking`.
pub trait CredentialStore: Send + Sync {
    /// Returns the secret stored for `account`, or `None` when there is none.
    ///
    /// # Errors
    /// Fails when the store cannot be read.
    fn get(&self, account: &str) -> Result<Option<String>, String>;

    /// Stores `secret` for `account`, replacing any existing secret.
    ///
    /// # Errors
    /// Fails when the store cannot be written.
    fn set(&self, account: &str, secret: &str) -> Result<(), String>;

    /// Removes the secret for `account`. Returns whether a secret existed.
    ///
    /// # Errors
    /// Fails when the store cannot be written.
    fn delete(&self, account: &str) -> Result<bool, String>;
}
