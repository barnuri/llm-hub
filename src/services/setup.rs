pub mod claude_setup;
pub mod codex_setup;
pub mod config_paths;
pub mod copilot_cli_setup;
pub mod json_config;
pub mod opencode_setup;
pub mod pi_setup;
pub mod setup_plan;

use self::claude_setup::ClaudeSetup;
use self::codex_setup::CodexSetup;
use self::copilot_cli_setup::CopilotCliSetup;
use self::opencode_setup::OpencodeSetup;
use self::pi_setup::PiSetup;
use self::setup_plan::SetupOutcome;
use super::launch::harness::Harness;
use super::launch::hub_client::HubClient;
use super::launch::hub_endpoint::HubEndpoint;

/// `llm-hub setup <harness> [--dry-run]`: writes the harness's own user config so it uses the
/// hub without `llm-hub <harness>`.
#[derive(Debug, PartialEq, Eq)]
pub struct SetupCommand {
    pub harness: Harness,
    pub dry_run: bool,
}

impl SetupCommand {
    const USAGE: &str = "usage: llm-hub setup <claude|codex|opencode|copilot-cli|pi> [--dry-run]";
    const DRY_RUN_FLAG: &str = "--dry-run";

    /// Reads the arguments after `llm-hub`. Returns `None` when the first one is not `setup`.
    ///
    /// # Errors
    /// Returns the usage line when the harness is missing or unknown, or a flag is unknown.
    pub fn parse(args: &[String]) -> Result<Option<Self>, String> {
        if args.first().map(String::as_str) != Some("setup") {
            return Ok(None);
        }
        let mut harness = None;
        let mut dry_run = false;
        for arg in &args[1..] {
            if arg == Self::DRY_RUN_FLAG {
                dry_run = true;
            } else if harness.is_none() {
                harness = Harness::from_command(arg);
                if harness.is_none() {
                    return Err(format!("unknown agent `{arg}`\n{}", Self::USAGE));
                }
            } else {
                return Err(format!("unexpected argument `{arg}`\n{}", Self::USAGE));
            }
        }
        let harness = harness.ok_or_else(|| Self::USAGE.to_string())?;
        Ok(Some(Self { harness, dry_run }))
    }
}

/// Checks that the hub answers, then writes (or with `--dry-run`, shows) the harness config.
///
/// # Errors
/// Returns a message when the hub is unreachable or the config cannot be read, merged or written.
pub async fn run(command: SetupCommand) -> Result<(), String> {
    let outcome = prepare(command.harness, |name| std::env::var(name).ok()).await?;
    println!("{}", outcome.apply(command.dry_run)?);
    Ok(())
}

async fn prepare(
    harness: Harness,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<SetupOutcome, String> {
    let endpoint = HubEndpoint::from_lookup(&lookup);
    let client = HubClient::new(endpoint)?;
    client.check_reachable().await?;
    let endpoint = client.endpoint();
    match harness {
        Harness::Claude => {
            ClaudeSetup::plan(&lookup, endpoint, client.claude_settings().await.as_deref())
        }
        Harness::Codex => CodexSetup::plan(&lookup, endpoint),
        Harness::Opencode => OpencodeSetup::plan(&lookup, endpoint, &client.models().await),
        Harness::CopilotCli => Ok(CopilotCliSetup::outcome(endpoint)),
        Harness::Pi => PiSetup::plan(&lookup, endpoint, &client.models().await),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn parse(args: &[&str]) -> Result<Option<SetupCommand>, String> {
        SetupCommand::parse(&args.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn setup_names_a_harness_and_an_optional_dry_run() {
        assert_eq!(
            parse(&["setup", "opencode"]),
            Ok(Some(SetupCommand {
                harness: Harness::Opencode,
                dry_run: false
            }))
        );
        assert_eq!(
            parse(&["setup", "--dry-run", "copilot-cli"]),
            Ok(Some(SetupCommand {
                harness: Harness::CopilotCli,
                dry_run: true
            }))
        );
    }

    #[test]
    fn other_commands_are_not_setup() {
        assert_eq!(parse(&["claude"]), Ok(None));
        assert_eq!(parse(&[]), Ok(None));
    }

    #[test]
    fn a_missing_unknown_or_extra_argument_is_an_error() {
        assert!(parse(&["setup"]).unwrap_err().starts_with("usage:"));
        assert!(parse(&["setup", "--dry-run"]).is_err());
        assert!(
            parse(&["setup", "vim"])
                .unwrap_err()
                .contains("unknown agent `vim`")
        );
        assert!(
            parse(&["setup", "pi", "codex"])
                .unwrap_err()
                .contains("unexpected argument")
        );
    }

    async fn hub() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"id": "copilot/m", "context_window": 128_000},
            ]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/claude-code/settings"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"model":"x"}"#))
            .mount(&server)
            .await;
        server
    }

    fn lookup_of(vars: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[tokio::test]
    async fn an_unreachable_hub_stops_the_setup() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let error = prepare(Harness::Pi, lookup_of(vec![("LLM_HUB_URL", server.uri())]))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            format!("llm-hub is not reachable at {}", server.uri())
        );
    }

    #[tokio::test]
    async fn every_harness_gets_an_outcome_from_a_reachable_hub() {
        let server = hub().await;
        let home = tempfile::tempdir().unwrap();
        let vars = vec![
            ("LLM_HUB_URL", server.uri()),
            ("HOME", home.path().to_string_lossy().into_owned()),
        ];

        for harness in [
            Harness::Claude,
            Harness::Codex,
            Harness::Opencode,
            Harness::CopilotCli,
            Harness::Pi,
        ] {
            let outcome = prepare(harness, lookup_of(vars.clone())).await.unwrap();
            let message = outcome.apply(true).unwrap();
            assert!(!message.is_empty(), "{harness:?}");
        }
    }
}
