use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Clone)]
pub struct UsageRow {
    pub ts_ms: u64,
    pub model: String,
    pub profile: String,
    pub status: u16,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Estimated USD at read time. Not persisted; old JSON store files load as `$0`.
    pub cost_usd: f64,
}

// UsageRow needs Deserialize only for the JSON store file.
impl<'de> Deserialize<'de> for UsageRow {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            ts_ms: u64,
            model: String,
            profile: String,
            status: u16,
            latency_ms: u64,
            #[serde(default)]
            ttft_ms: Option<u64>,
            tokens_in: u64,
            tokens_out: u64,
            #[serde(default)]
            cache_read_tokens: u64,
            #[serde(default)]
            cache_write_tokens: u64,
            #[serde(default)]
            cost_usd: f64,
        }
        let raw = Raw::deserialize(d)?;
        Ok(UsageRow {
            ts_ms: raw.ts_ms,
            model: raw.model,
            profile: raw.profile,
            status: raw.status,
            latency_ms: raw.latency_ms,
            ttft_ms: raw.ttft_ms,
            tokens_in: raw.tokens_in,
            tokens_out: raw.tokens_out,
            cache_read_tokens: raw.cache_read_tokens,
            cache_write_tokens: raw.cache_write_tokens,
            cost_usd: raw.cost_usd,
        })
    }
}
