//! GitHub Copilot's published per-model descriptions, fetched from GitHub Docs.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::services::copilot_prices::CopilotPrices;
use crate::utils::markdown_table;

/// The last fetched description table, keyed by [`CopilotPrices::normalize_id`].
#[derive(Debug)]
pub struct CopilotDescriptions {
    table: ArcSwap<HashMap<String, String>>,
}

impl Default for CopilotDescriptions {
    fn default() -> Self {
        Self {
            table: ArcSwap::from_pointee(HashMap::new()),
        }
    }
}

impl CopilotDescriptions {
    const MODEL_COLUMN: &str = "Model";
    const TASK_COLUMN: &str = "Task area";
    const EXCELS_COLUMN: &str = "Excels at";

    /// The fetched table. Empty until the first successful fetch.
    pub fn snapshot(&self) -> Arc<HashMap<String, String>> {
        self.table.load_full()
    }

    /// Replaces the table that [`Self::snapshot`] returns.
    fn store(&self, table: HashMap<String, String>) {
        self.table.store(Arc::new(table));
    }

    /// Stores a fetched table that differs from the current one and reports whether it did.
    /// A failed fetch keeps the last table.
    pub fn apply(&self, fetched: Result<HashMap<String, String>, String>) -> bool {
        let table = match fetched {
            Ok(table) => table,
            Err(error) => {
                tracing::warn!(%error, "copilot description fetch failed, keeping the last table");
                return false;
            }
        };
        if *self.snapshot() == table {
            return false;
        }
        tracing::info!(models = table.len(), "copilot model descriptions updated");
        self.store(table);
        true
    }

    /// The descriptions on the models page, or an error when the page or its table is unusable.
    pub async fn fetch(
        client: &reqwest::Client,
        url: &str,
    ) -> Result<HashMap<String, String>, String> {
        let body = CopilotPrices::fetch_body(client, url).await?;
        let descriptions = Self::parse(&body);
        if descriptions.is_empty() {
            return Err("the models page has no descriptions".into());
        }
        Ok(descriptions)
    }

    /// `<Task area>. <Excels at>` for every row of a table with Model, Task area and Excels at
    /// columns. The first row for a model wins; a row with neither text is skipped.
    pub fn parse(markdown: &str) -> HashMap<String, String> {
        let mut descriptions = HashMap::new();
        markdown_table::for_each_row(markdown, |columns, cells| {
            if let Some((id, text)) = Self::description_row(columns, cells) {
                descriptions.entry(id).or_insert(text);
            }
        });
        descriptions
    }

    fn description_row(columns: &[String], cells: &[String]) -> Option<(String, String)> {
        if Self::is_filler_row(cells) {
            return None;
        }
        // Header notes such as `(primary use case)` are not part of the column name.
        let columns: Vec<String> = columns
            .iter()
            .map(|column| CopilotPrices::model_name(column))
            .collect();
        let cell = |name: &str| markdown_table::cell(&columns, cells, name);
        let id = CopilotPrices::normalize_id(&CopilotPrices::model_name(cell(Self::MODEL_COLUMN)?));
        if id.is_empty() {
            return None;
        }
        let text = Self::join(cell(Self::TASK_COLUMN)?, cell(Self::EXCELS_COLUMN)?)?;
        Some((id, text))
    }

    /// A spacer row has only empty cells; a separator row has only `-` and `:` characters.
    fn is_filler_row(cells: &[String]) -> bool {
        cells
            .iter()
            .all(|cell| cell.chars().all(|c| c == '-' || c == ':'))
    }

    fn join(task: &str, excels: &str) -> Option<String> {
        match (task.is_empty(), excels.is_empty()) {
            (true, true) => None,
            (false, true) => Some(task.to_string()),
            (true, false) => Some(excels.to_string()),
            (false, false) if task.ends_with('.') => Some(format!("{task} {excels}")),
            (false, false) => Some(format!("{task}. {excels}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r"
## Comparison of AI models for GitHub Copilot

### Recommended models by task

| Model                                 | Task area                                 | Excels at (primary use case)                                       | Further reading                                    |
| ------------------------------------- | ----------------------------------------- | ------------------------------------------------------------------ | -------------------------------------------------- |
|                                       |                                           |                                                                    |                                                    |
| GPT-6 Luna                            | Fast help with simple or repetitive tasks | Quick, cost-efficient responses for smaller, faster tasks          | [GPT-6 Luna model card](https://example.test/luna) |
|                                       |                                           |                                                                    |                                                    |
| Claude Opus 4.8 (fast mode) (preview) | Deep reasoning and debugging              | Complex problem-solving challenges, sophisticated reasoning        | [Opus model card](https://example.test/opus)       |
| Claude Sonnet 5.5                     |                                           | Efficient task completion with fewer steps, tokens, and tool calls | TBD                                                |
| Claude Haiku 5.5                      | Fast help with simple or repetitive tasks. |                                                                   | TBD                                                |
| Claude Opus 5                         | Deep reasoning and debugging.             | Complex problem-solving challenges                                 | TBD                                                |
| Mystery Model                         |                                           |                                                                    | TBD                                                |
| GPT-6 Luna                            | A duplicate row                           | Should not replace the first row                                   | TBD                                                |

### Quick facts

| Model        | Description                         |
| ------------ | ----------------------------------- |
| GPT-5.6 Luna | Lightweight, cost-efficient option. |

| Model      | Input | Output |
| ---------- | ----: | -----: |
| GPT-6 Luna | $0.10 | $0.50  |
";

    #[test]
    fn parse_joins_task_area_and_excels_at_for_each_model_row() {
        assert_eq!(
            CopilotDescriptions::parse(SAMPLE),
            HashMap::from([
                (
                    "gpt-6-luna".into(),
                    "Fast help with simple or repetitive tasks. Quick, cost-efficient responses for smaller, faster tasks".into()
                ),
                (
                    "claude-opus-4-8-fast".into(),
                    "Deep reasoning and debugging. Complex problem-solving challenges, sophisticated reasoning".into()
                ),
                (
                    "claude-sonnet-5-5".into(),
                    "Efficient task completion with fewer steps, tokens, and tool calls".into()
                ),
                (
                    "claude-haiku-5-5".into(),
                    "Fast help with simple or repetitive tasks.".into()
                ),
                (
                    "claude-opus-5".into(),
                    "Deep reasoning and debugging. Complex problem-solving challenges".into()
                ),
            ])
        );
    }

    #[test]
    fn parse_skips_spacer_rows_separator_rows_and_rows_without_text() {
        let descriptions = CopilotDescriptions::parse(SAMPLE);
        assert!(!descriptions.contains_key("mystery-model"));
        assert!(
            descriptions
                .keys()
                .all(|id| id.chars().any(char::is_alphanumeric))
        );
    }

    #[test]
    fn parse_ignores_tables_without_a_task_area_column() {
        let tables = "| Model | Description |\n| --- | --- |\n| GPT-5.6 Luna | Lightweight |\n\n| Model | Input | Output |\n| --- | ---: | ---: |\n| GPT-6 Luna | $0.10 | $0.50 |\n";
        assert!(CopilotDescriptions::parse(tables).is_empty());
    }

    #[test]
    fn parse_keeps_the_first_row_of_a_duplicate_model() {
        assert!(CopilotDescriptions::parse(SAMPLE)["gpt-6-luna"].starts_with("Fast help"));
    }

    #[test]
    fn parse_of_a_page_without_tables_is_empty() {
        assert!(CopilotDescriptions::parse("# Comparison\n\nNo tables today.").is_empty());
    }

    #[test]
    fn snapshot_is_empty_until_a_table_is_stored() {
        let descriptions = CopilotDescriptions::default();
        assert!(descriptions.snapshot().is_empty());
        descriptions.store(HashMap::from([("gpt-6-luna".into(), "Fast".into())]));
        assert_eq!(descriptions.snapshot()["gpt-6-luna"], "Fast");
    }

    #[test]
    fn apply_stores_a_changed_table_and_keeps_the_last_one_on_error_or_no_change() {
        let descriptions = CopilotDescriptions::default();
        let table = HashMap::from([("gpt-6-luna".to_string(), "Fast".to_string())]);
        assert!(descriptions.apply(Ok(table.clone())));
        assert!(!descriptions.apply(Ok(table.clone())));
        assert!(!descriptions.apply(Err("status 503".into())));
        assert_eq!(*descriptions.snapshot(), table);
    }

    #[tokio::test]
    async fn fetch_rejects_an_error_status_and_a_page_without_descriptions() {
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
            CopilotDescriptions::fetch(&client, &url)
                .await
                .unwrap_err()
                .contains("503")
        );
        assert!(CopilotDescriptions::fetch(&client, &url).await.is_err());
        assert_eq!(
            CopilotDescriptions::fetch(&client, &url)
                .await
                .unwrap()
                .len(),
            5
        );
    }
}
