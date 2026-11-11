/// Key prefix of a Bifrost virtual key. A profile whose key starts with it uses the Bifrost
/// quota endpoint unless `LLM_HUB_<NAME>_USAGE` says otherwise.
pub const BIFROST_KEY_PREFIX: &str = "sk-bf-";

/// Where a non-Copilot profile's quota is read from. Copilot profiles need no entry: their
/// `copilot` auth is the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    Bifrost,
}

impl UsageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageSource::Bifrost => "bifrost",
        }
    }
}
