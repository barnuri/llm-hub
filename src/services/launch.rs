pub mod claude_launcher;
pub mod codex_launcher;
pub mod copilot_cli_launcher;
pub mod harness;
pub mod hub_client;
pub mod hub_endpoint;
pub mod opencode_launcher;
pub mod pi_launcher;

use self::claude_launcher::ClaudeLauncher;
use self::codex_launcher::CodexLauncher;
use self::copilot_cli_launcher::CopilotCliLauncher;
use self::harness::Harness;
use self::hub_client::HubClient;
use self::hub_endpoint::HubEndpoint;
use self::opencode_launcher::OpencodeLauncher;
use self::pi_launcher::PiLauncher;
use crate::schemas::launch::hub_model::HubModel;
use crate::schemas::launch::launch_args::LaunchArgs;
use crate::schemas::launch::launch_spec::LaunchSpec;
use crate::schemas::launch::profile_filter::ProfileFilter;

/// `llm-hub [--profiles a,b] <harness> [--profiles a,b] [model] [args...]`.
#[derive(Debug, PartialEq, Eq)]
pub struct LaunchCommand {
    pub harness: Harness,
    pub profiles: ProfileFilter,
    pub args: Vec<String>,
}

impl LaunchCommand {
    /// Reads the arguments after `llm-hub`. `--profiles` before the harness
    /// name, or anywhere after it before `--`, is taken out of the harness args.
    /// Returns `None` when no harness is named.
    ///
    /// # Errors
    /// Returns a message for a bad `--profiles` value, or `--profiles`
    /// without a harness after it.
    pub fn parse(mut args: Vec<String>) -> Result<Option<Self>, String> {
        let mut profiles = ProfileFilter::take_leading_flags(&mut args)?;
        let Some(harness) = args.first().and_then(|first| Harness::from_command(first)) else {
            if profiles.is_empty() {
                return Ok(None);
            }
            return Err("--profiles must come with an agent command, e.g. llm-hub --profiles copilot claude".to_string());
        };
        args.remove(0);
        profiles.merge(ProfileFilter::take_flags(&mut args)?);
        Ok(Some(Self {
            harness,
            profiles,
            args,
        }))
    }
}

/// Checks that the hub answers, then replaces this process with the harness.
///
/// # Errors
/// Returns a message when the hub is unreachable, the pi extension cannot be
/// written, or the harness binary cannot start.
pub async fn run(command: LaunchCommand) -> Result<(), String> {
    let endpoint = HubEndpoint::from_lookup(|name| std::env::var(name).ok());
    let client = HubClient::new(endpoint)?.with_profiles(command.profiles);
    client.check_reachable().await?;
    let spec = build_spec(command.harness, &client, LaunchArgs::parse(command.args)).await?;
    spec.exec()
}

async fn build_spec(
    harness: Harness,
    client: &HubClient,
    args: LaunchArgs,
) -> Result<LaunchSpec, String> {
    let endpoint = client.endpoint();
    let spec = match harness {
        Harness::Claude => {
            let inherited = std::env::var(ClaudeLauncher::custom_headers_var()).ok();
            ClaudeLauncher::spec(
                endpoint,
                args,
                client.claude_settings().await,
                client.profiles(),
                inherited.as_deref(),
            )
        }
        Harness::Codex => {
            let catalog = catalog_for(client, &args).await;
            CodexLauncher::spec(endpoint, args, &catalog)
        }
        Harness::Opencode => OpencodeLauncher::spec(endpoint, args, &client.models().await),
        Harness::CopilotCli => {
            let catalog = catalog_for(client, &args).await;
            CopilotCliLauncher::spec(endpoint, args, &catalog)
        }
        Harness::Pi => {
            let extension = PiLauncher::write_extension(|name| std::env::var(name).ok())?;
            PiLauncher::spec(endpoint, args, &extension, client.profiles())
        }
    };
    Ok(spec)
}

/// The model catalog is only read when a model was named, to size its limits.
async fn catalog_for(client: &HubClient, args: &LaunchArgs) -> Vec<HubModel> {
    if args.model().is_none() {
        return Vec::new();
    }
    client.models().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<LaunchCommand>, String> {
        LaunchCommand::parse(args.iter().map(ToString::to_string).collect())
    }

    fn parsed(args: &[&str]) -> LaunchCommand {
        parse(args).unwrap().unwrap()
    }

    #[test]
    fn global_profiles_flag_comes_before_the_harness() {
        let command = parsed(&[
            "--profiles",
            "copilot,ivrit",
            "claude",
            "copilot/auto",
            "-p",
        ]);

        assert_eq!(command.harness, Harness::Claude);
        assert_eq!(command.profiles.to_list(), "copilot,ivrit");
        assert_eq!(command.args, ["copilot/auto", "-p"]);
    }

    #[test]
    fn profiles_flag_after_the_harness_is_consumed() {
        let command = parsed(&[
            "claude",
            "--profiles",
            "copilot,ivrit",
            "copilot/auto",
            "-c",
        ]);

        assert_eq!(command.profiles.to_list(), "copilot,ivrit");
        assert_eq!(command.args, ["copilot/auto", "-c"]);
        assert_eq!(
            LaunchArgs::parse(command.args).model(),
            Some("copilot/auto")
        );
    }

    #[test]
    fn equals_form_works_in_both_positions_and_merges() {
        let command = parsed(&["--profiles=copilot", "codex", "--profiles=ivrit", "m"]);

        assert_eq!(command.harness, Harness::Codex);
        assert_eq!(command.profiles.to_list(), "copilot,ivrit");
        assert_eq!(command.args, ["m"]);
    }

    #[test]
    fn no_flag_means_no_filter() {
        let command = parsed(&["pi", "m", "--offline"]);

        assert!(command.profiles.is_empty());
        assert_eq!(command.args, ["m", "--offline"]);
    }

    #[test]
    fn non_agent_commands_are_not_launches() {
        assert_eq!(parse(&["service", "status"]), Ok(None));
        assert_eq!(parse(&[]), Ok(None));
    }

    #[test]
    fn profiles_without_an_agent_command_is_an_error() {
        assert!(parse(&["--profiles", "copilot"]).is_err());
        assert!(parse(&["--profiles", "copilot", "serve"]).is_err());
    }

    #[test]
    fn profiles_without_a_value_is_an_error() {
        assert!(parse(&["claude", "--profiles"]).is_err());
        assert!(parse(&["--profiles", "claude"]).is_err());
    }
}
