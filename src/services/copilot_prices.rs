//! GitHub Copilot's published per-model token prices, fetched from GitHub Docs.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::configs::TokenRates;
use crate::consts::{
    COPILOT_DESCRIPTIONS_URL, COPILOT_PRICES_MIN_GAP, COPILOT_PRICES_TIMEOUT, COPILOT_PRICES_URL,
    LONG_CONTEXT_SUFFIX,
};
use crate::dependencies::state::AppState;
use crate::services::copilot_descriptions::CopilotDescriptions;
use crate::utils::markdown_table;

/// The last fetched price table, keyed by [`CopilotPrices::normalize_id`].
#[derive(Debug)]
pub struct CopilotPrices {
    table: ArcSwap<HashMap<String, TokenRates>>,
    wake: Notify,
}

impl Default for CopilotPrices {
    fn default() -> Self {
        Self {
            table: ArcSwap::from_pointee(HashMap::new()),
            wake: Notify::new(),
        }
    }
}

impl CopilotPrices {
    const DEFAULT_TIER: &str = "Default";
    const FAST_MODE: &str = "(fast mode)";
    const MODEL_COLUMN: &str = "Model";
    const TIER_COLUMN: &str = "Tier";
    const INPUT_COLUMN: &str = "Input";
    const CACHED_INPUT_COLUMN: &str = "Cached input";
    const CACHE_WRITE_COLUMN: &str = "Cache write";
    const OUTPUT_COLUMN: &str = "Output";

    /// The fetched table. Empty until the first successful fetch.
    pub fn snapshot(&self) -> Arc<HashMap<String, TokenRates>> {
        self.table.load_full()
    }

    /// Asks the refresh loop for an early fetch. The loop still keeps the minimum gap.
    pub fn request_refresh(&self) {
        self.wake.notify_one();
    }

    /// `Claude Opus 4.8`, `claude-opus-4.8` and `claude-opus-4-8[1m]` all become `claude-opus-4-8`.
    pub fn normalize_id(raw: &str) -> String {
        let trimmed = raw.trim();
        let bare = trimmed.strip_suffix(LONG_CONTEXT_SUFFIX).unwrap_or(trimmed);
        bare.to_ascii_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
            .replace('.', "-")
    }

    /// Default-tier prices from every Markdown table with Model, Input and Output columns.
    /// The first row for a model wins; a row with an unreadable Input or Output is skipped.
    pub fn parse(markdown: &str) -> HashMap<String, TokenRates> {
        let mut prices = HashMap::new();
        markdown_table::for_each_row(markdown, |columns, cells| {
            if let Some((id, rates)) = Self::price_row(columns, cells) {
                prices.entry(id).or_insert(rates);
            }
        });
        prices
    }

    /// Fetches both pages at startup and every `copilot_prices_refresh` or on request, at least
    /// [`COPILOT_PRICES_MIN_GAP`] apart, while a Copilot profile is enabled and the refresh is on.
    pub fn spawn_refresh_loop(state: AppState) {
        tokio::spawn(async move {
            let client = match reqwest::Client::builder()
                .timeout(COPILOT_PRICES_TIMEOUT)
                .build()
            {
                Ok(client) => client,
                Err(error) => {
                    tracing::warn!(%error, "copilot price refresh disabled: no http client");
                    return;
                }
            };
            let mut last_fetch: Option<Instant> = None;
            loop {
                if let Some(last) = last_fetch {
                    tokio::time::sleep_until(last + COPILOT_PRICES_MIN_GAP).await;
                }
                let interval = state.config().copilot_prices_refresh;
                if interval.is_some() && Self::has_enabled_copilot(&state) {
                    last_fetch = Some(Instant::now());
                    Self::refresh_once(
                        &state,
                        &client,
                        COPILOT_PRICES_URL,
                        COPILOT_DESCRIPTIONS_URL,
                    )
                    .await;
                }
                tokio::select! {
                    () = tokio::time::sleep(interval.unwrap_or(COPILOT_PRICES_MIN_GAP)) => {}
                    () = state.copilot_prices().wake.notified() => {}
                }
            }
        });
    }

    fn price_row(columns: &[String], cells: &[String]) -> Option<(String, TokenRates)> {
        let cell = |name: &str| markdown_table::cell(columns, cells, name);
        if cell(Self::TIER_COLUMN).is_some_and(|tier| tier != Self::DEFAULT_TIER) {
            return None;
        }
        let name = Self::model_name(cell(Self::MODEL_COLUMN)?);
        if name.is_empty() {
            return None;
        }
        let rates = TokenRates {
            input_per_1m: Self::usd(cell(Self::INPUT_COLUMN)?)?,
            output_per_1m: Self::usd(cell(Self::OUTPUT_COLUMN)?)?,
            cache_read_per_1m: cell(Self::CACHED_INPUT_COLUMN)
                .and_then(Self::usd)
                .unwrap_or(0.0),
            cache_write_per_1m: cell(Self::CACHE_WRITE_COLUMN)
                .and_then(Self::usd)
                .unwrap_or(0.0),
        };
        Some((Self::normalize_id(&name), rates))
    }

    /// Drops footnotes (`[^note]`) and notes in parentheses, except `(fast mode)`, which becomes `fast`.
    pub(crate) fn model_name(raw: &str) -> String {
        let mut name = String::with_capacity(raw.len());
        let mut rest = raw;
        while let Some(start) = rest.find(['(', '[']) {
            name.push_str(&rest[..start]);
            let close = if rest[start..].starts_with('(') {
                ')'
            } else {
                ']'
            };
            let Some(end) = rest[start..].find(close) else {
                rest = "";
                break;
            };
            let group = &rest[start..=start + end];
            if group.eq_ignore_ascii_case(Self::FAST_MODE) {
                name.push_str(" fast ");
            }
            rest = &rest[start + end + 1..];
        }
        name.push_str(rest);
        name.trim().to_string()
    }

    /// `$1.75` → 1.75. Text such as `Not applicable` is not a price.
    fn usd(cell: &str) -> Option<f64> {
        cell.trim().strip_prefix('$')?.replace(',', "").parse().ok()
    }

    fn has_enabled_copilot(state: &AppState) -> bool {
        state
            .config()
            .profiles
            .iter()
            .any(|profile| profile.enabled && profile.copilot.is_some())
    }

    /// One fetch of each page. A failed fetch keeps that page's last table; a changed table
    /// relabels the model list.
    async fn refresh_once(
        state: &AppState,
        client: &reqwest::Client,
        prices_url: &str,
        descriptions_url: &str,
    ) {
        let (prices, descriptions) = tokio::join!(
            Self::fetch(client, prices_url),
            CopilotDescriptions::fetch(client, descriptions_url)
        );
        let prices_changed = Self::apply(state, prices);
        let descriptions_changed = state.copilot_descriptions().apply(descriptions);
        if prices_changed || descriptions_changed {
            crate::routes::models::refresh_models(state).await;
        }
    }

    fn apply(state: &AppState, fetched: Result<HashMap<String, TokenRates>, String>) -> bool {
        let fetched = match fetched {
            Ok(fetched) => fetched,
            Err(error) => {
                tracing::warn!(%error, "copilot price fetch failed, keeping the last table");
                return false;
            }
        };
        if *state.copilot_prices().snapshot() == fetched {
            return false;
        }
        tracing::info!(models = fetched.len(), "copilot prices updated");
        state.copilot_prices().table.store(Arc::new(fetched));
        true
    }

    pub(crate) async fn fetch_body(client: &reqwest::Client, url: &str) -> Result<String, String> {
        let response = client.get(url).send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("status {status}"));
        }
        response.text().await.map_err(|e| e.to_string())
    }

    async fn fetch(
        client: &reqwest::Client,
        url: &str,
    ) -> Result<HashMap<String, TokenRates>, String> {
        let body = Self::fetch_body(client, url).await?;
        let prices = Self::parse(&body);
        if prices.is_empty() {
            return Err("the pricing page has no price table".into());
        }
        Ok(prices)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::configs::HubConfig;
    use crate::services::fake_gh_cli::FakeGhCli;
    use crate::services::gh_cli::GhOutcome;
    use crate::services::memory_credential_store::MemoryCredentialStore;

    const SAMPLE: &str = r"
### OpenAI

| Model         | Release status | Category    | Tier         | Threshold (input tokens) |  Input | Cached input |    Cache write | Output |
| ------------- | -------------- | ----------- | ------------ | ------------------------ | -----: | -----------: | -------------: | -----: |
|               |                |             |              |                          |        |              |                |        |
| GPT-5.3-Codex | GA             | Powerful    | Default      | Not applicable           |  $1.75 |       $0.175 | Not applicable | $14.00 |
| GPT-6.1 Sol   | GA             | Powerful    | Default      | ≤ 272K                   |  $2.00 |        $0.10 |          $2.50 | $10.00 |
| GPT-6.1 Sol   | GA             | Powerful    | Long context | > 272K                   |  $4.00 |        $0.20 |          $5.00 | $15.00 |

| Model                                 | Release status | Category | Tier    | Threshold (input tokens) |  Input | Cached input | Cache write | Output |
| ------------------------------------- | -------------- | -------- | ------- | ------------------------ | -----: | -----------: | ----------: | -----: |
| Claude Opus 4.8                       | GA             | Powerful | Default | Not applicable           |  $5.00 |        $0.50 |       $6.25 | $25.00 |
| Claude Opus 4.8 (fast mode) (preview) | GA             | Powerful | Default | Not applicable           | $10.00 |        $1.00 |      $12.50 | $50.00 |

| Model                                 | Release status | Category  | Tier    | Threshold (input tokens) | Input | Cached input | Output |
| ------------------------------------- | -------------- | --------- | ------- | ------------------------ | ----: | -----------: | -----: |
| Gemini 3.8 Flash[^gemini-flash-promo] | GA             | Versatile | Default | Not applicable           | $0.75 |       $0.075 |  $3.75 |

| Model              | Release status | Category    | Input | Cached input | Output |
| ------------------ | -------------- | ----------- | ----: | -----------: | -----: |
| MAI-Code-1.1-Flash | GA             | Lightweight | $0.20 |        $0.02 |  $1.20 |

| Model               | Release status | Text input | Cached text input | Image output |
| ------------------- | -------------- | ---------: | ----------------: | -----------: |
| GPT Image 2.5 Flare | Public preview |      $5.00 |             $1.25 |       $30.00 |
";

    fn rates(input: f64, cache_read: f64, cache_write: f64, output: f64) -> TokenRates {
        TokenRates {
            input_per_1m: input,
            output_per_1m: output,
            cache_read_per_1m: cache_read,
            cache_write_per_1m: cache_write,
        }
    }

    #[test]
    fn parse_reads_default_tier_rows_of_every_price_table() {
        let prices = CopilotPrices::parse(SAMPLE);
        assert_eq!(
            prices,
            HashMap::from([
                ("gpt-5-3-codex".into(), rates(1.75, 0.175, 0.0, 14.0)),
                ("gpt-6-1-sol".into(), rates(2.0, 0.10, 2.50, 10.0)),
                ("claude-opus-4-8".into(), rates(5.0, 0.50, 6.25, 25.0)),
                ("claude-opus-4-8-fast".into(), rates(10.0, 1.0, 12.50, 50.0)),
                ("gemini-3-8-flash".into(), rates(0.75, 0.075, 0.0, 3.75)),
                ("mai-code-1-1-flash".into(), rates(0.20, 0.02, 0.0, 1.20)),
            ])
        );
    }

    #[test]
    fn parse_of_a_page_without_tables_is_empty() {
        assert!(CopilotPrices::parse("# Pricing\n\nNo tables today.").is_empty());
    }

    #[test]
    fn normalize_id_matches_doc_names_and_copilot_ids() {
        assert_eq!(
            CopilotPrices::normalize_id("Claude Opus 4.8"),
            "claude-opus-4-8"
        );
        assert_eq!(
            CopilotPrices::normalize_id("claude-opus-4.8"),
            "claude-opus-4-8"
        );
        assert_eq!(
            CopilotPrices::normalize_id("claude-opus-4-8[1m]"),
            "claude-opus-4-8"
        );
        assert_eq!(CopilotPrices::normalize_id("GPT-5.4 nano"), "gpt-5-4-nano");
    }

    #[tokio::test]
    async fn fetch_rejects_an_error_status_and_a_page_without_prices() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("no tables"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = server.uri();
        assert!(
            CopilotPrices::fetch(&client, &url)
                .await
                .unwrap_err()
                .contains("503")
        );
        assert!(CopilotPrices::fetch(&client, &url).await.is_err());
        assert_eq!(CopilotPrices::fetch(&client, &url).await.unwrap().len(), 6);
    }

    #[tokio::test]
    async fn request_refresh_wakes_a_waiting_loop() {
        let prices = CopilotPrices::default();
        prices.request_refresh();
        tokio::time::timeout(Duration::from_millis(100), prices.wake.notified())
            .await
            .expect("a requested refresh wakes the loop");
    }

    const DESCRIPTIONS_SAMPLE: &str = "| Model | Task area | Excels at |\n| --- | --- | --- |\n| GPT-6 Luna | Fast help | Quick answers |\n";

    fn state_with_a_dead_copilot_upstream() -> AppState {
        let vars = HashMap::from([
            ("LLM_HUB_PROFILES".to_string(), "copilot".to_string()),
            (
                "LLM_HUB_COPILOT_BASE_URL".to_string(),
                "http://127.0.0.1:9".to_string(),
            ),
        ]);
        AppState::with_seams(
            HubConfig::from_map(&vars).unwrap(),
            None,
            Arc::new(MemoryCredentialStore::default()),
            Arc::new(FakeGhCli::new(GhOutcome::NotInstalled)),
        )
        .unwrap()
    }

    async fn serve_pages(prices_status: u16, descriptions_status: u16) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let page = |status: u16, body: &str| {
            ResponseTemplate::new(status).set_body_string(if status == 200 { body } else { "" })
        };
        Mock::given(method("GET"))
            .and(path("/prices"))
            .respond_with(page(prices_status, SAMPLE))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/descriptions"))
            .respond_with(page(descriptions_status, DESCRIPTIONS_SAMPLE))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn refresh_once_stores_both_pages() {
        let server = serve_pages(200, 200).await;
        let state = state_with_a_dead_copilot_upstream();
        CopilotPrices::refresh_once(
            &state,
            &reqwest::Client::new(),
            &format!("{}/prices", server.uri()),
            &format!("{}/descriptions", server.uri()),
        )
        .await;
        assert_eq!(state.copilot_prices().snapshot().len(), 6);
        assert_eq!(
            state.copilot_descriptions().snapshot()["gpt-6-luna"],
            "Fast help. Quick answers"
        );
    }

    #[tokio::test]
    async fn refresh_once_keeps_the_prices_when_only_the_descriptions_page_fails() {
        let server = serve_pages(200, 503).await;
        let state = state_with_a_dead_copilot_upstream();
        CopilotPrices::refresh_once(
            &state,
            &reqwest::Client::new(),
            &format!("{}/prices", server.uri()),
            &format!("{}/descriptions", server.uri()),
        )
        .await;
        assert_eq!(state.copilot_prices().snapshot().len(), 6);
        assert!(state.copilot_descriptions().snapshot().is_empty());
    }
}
