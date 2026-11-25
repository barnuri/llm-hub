use serde::{Deserialize, Serialize};

/// Where a request row's cache-write count comes from. `Reported` rows carry the upstream's own
/// count in `cache_write_tokens`. `UncachedInput` rows had no reported write, so the row stores the
/// uncached input (prompt minus cache reads) as an upper-bound estimate in `uncached_input_tokens`
/// and leaves `cache_write_tokens` at 0. `None` rows have nothing to count: a failure or an empty prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheWriteSource {
    Reported,
    UncachedInput,
    #[default]
    None,
}

impl CacheWriteSource {
    /// The stored text, the same as the serde name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CacheWriteSource::Reported => "reported",
            CacheWriteSource::UncachedInput => "uncached_input",
            CacheWriteSource::None => "none",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CacheWriteSource;

    #[test]
    fn stored_text_matches_the_serde_name() {
        for source in [
            CacheWriteSource::Reported,
            CacheWriteSource::UncachedInput,
            CacheWriteSource::None,
        ] {
            assert_eq!(
                serde_json::to_value(source).unwrap(),
                serde_json::json!(source.as_str())
            );
        }
    }
}
