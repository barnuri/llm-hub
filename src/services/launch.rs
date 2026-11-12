pub mod claude_launcher;
pub mod codex_launcher;
pub mod copilot_cli_launcher;
pub mod harness;
pub mod hub_client;
pub mod hub_endpoint;
pub mod opencode_launcher;
pub mod pi_launcher;

use std::collections::BTreeMap;

use self::claude_launcher::ClaudeLauncher;
use self::codex_launcher::CodexLauncher;
use self::copilot_cli_launcher::CopilotCliLauncher;
use self::harness::Harness;
use self::hub_client::HubClient;
use self::hub_endpoint::HubEndpoint;
use self::opencode_launcher::OpencodeLauncher;
use self::pi_launcher::PiLauncher;
use crate::schemas::hub_selector::HubSelector;
use crate::schemas::launch::hub_model::HubModel;
use crate::schemas::launch::launch_args::LaunchArgs;
use crate::schemas::launch::launch_spec::LaunchSpec;
use crate::schemas::launch::profile_filter::ProfileFilter;
use crate::schemas::launch::smart_auto_flags::SmartAutoFlags;

/// `llm-hub [--profiles a,b] [--smart-auto-*] <harness> [...] [model] [args...]`.
#[derive(Debug, PartialEq, Eq)]
pub struct LaunchCommand {
    pub harness: Harness,
    pub profiles: ProfileFilter,
    pub smart_auto: SmartAutoFlags,
    pub args: Vec<String>,
}

impl LaunchCommand {
    /// Reads the arguments after `llm-hub`. `--profiles` and `--smart-auto-*` before the harness
    /// name, or anywhere after it before `--`, are taken out of the harness args.
    /// Returns `None` when no harness is named.
    ///
    /// # Errors
    /// Returns a message for a bad flag value, or a flag without a harness after it.
    pub fn parse(mut args: Vec<String>) -> Result<Option<Self>, String> {
        let mut profiles = ProfileFilter::default();
        let mut smart_auto = SmartAutoFlags::default();
        loop {
            let before = args.len();
            profiles.merge(ProfileFilter::take_leading_flags(&mut args)?);
            smart_auto.take_leading_flags(&mut args)?;
            if args.len() == before {
                break;
            }
        }
        let Some(harness) = args.first().and_then(|first| Harness::from_command(first)) else {
            if profiles.is_empty() && smart_auto.is_empty() {
                return Ok(None);
            }
            return Err("--profiles and --smart-auto-* must come with an agent command, e.g. llm-hub --profiles copilot claude".to_string());
        };
        args.remove(0);
        profiles.merge(ProfileFilter::take_flags(&mut args)?);
        smart_auto.take_flags(&mut args)?;
        Ok(Some(Self {
            harness,
            profiles,
            smart_auto,
            args,
        }))
    }

    /// A tier flag must name the tier of a `hub/smart-auto-<tier>` model, as the proxy refuses it otherwise.
    fn check_tier_matches(model: &str, smart_auto: &SmartAutoFlags) -> Result<(), String> {
        let Some(header_tier) = smart_auto.tier() else {
            return Ok(());
        };
        match HubSelector::parse(model, &BTreeMap::new()).ok().flatten() {
            Some(selector) if selector.tier != header_tier => Err(format!(
                "tier header `{}` conflicts with model `{}`: use the matching model id or remove the header",
                header_tier.as_str(),
                model,
            )),
            _ => Ok(()),
        }
    }
}

/// Checks that the hub answers, then replaces this process with the harness.
///
/// # Errors
/// Returns a message when the hub is unreachable, the pi extension cannot be
/// written, the harness binary cannot start, or the Smart Auto flags do not fit the harness.
pub async fn run(command: LaunchCommand) -> Result<(), String> {
    let LaunchCommand {
        harness,
        profiles,
        smart_auto,
        args,
    } = command;
    let endpoint = HubEndpoint::from_lookup(|name| std::env::var(name).ok());
    let client = HubClient::new(endpoint)?.with_profiles(profiles);
    client.check_reachable().await?;
    let spec = build_spec(harness, &client, LaunchArgs::parse(args), &smart_auto).await?;
    spec.exec()
}

async fn build_spec(
    harness: Harness,
    client: &HubClient,
    args: LaunchArgs,
    smart_auto: &SmartAutoFlags,
) -> Result<LaunchSpec, String> {
    let endpoint = client.endpoint();
    let spec = match harness {
        Harness::Claude => {
            let inherited = std::env::var(ClaudeLauncher::custom_headers_var()).ok();
            let model = ClaudeLauncher::inherited_model(|name| std::env::var(name).ok());
            let args = header_transport_model(args, smart_auto, model.as_deref())?;
            ClaudeLauncher::spec(
                endpoint,
                args,
                client.claude_settings().await,
                client.profiles(),
                smart_auto,
                inherited.as_deref(),
            )
        }
        Harness::Codex => {
            let args = preset_args(args, smart_auto)?;
            let catalog = catalog_for(client, &args).await;
            CodexLauncher::spec(endpoint, args, &catalog)
        }
        Harness::Opencode => {
            let args = header_transport_model(args, smart_auto, None)?;
            OpencodeLauncher::spec(
                endpoint,
                args,
                &client.models().await,
                client.profiles(),
                smart_auto,
            )
        }
        Harness::CopilotCli => {
            let args = preset_args(args, smart_auto)?;
            let catalog = catalog_for(client, &args).await;
            CopilotCliLauncher::spec(endpoint, args, &catalog)
        }
        Harness::Pi => {
            let args = preset_args(args, smart_auto)?;
            let extension = PiLauncher::write_extension(|name| std::env::var(name).ok())?;
            PiLauncher::spec(endpoint, args, &extension, client.profiles())
        }
    };
    Ok(spec)
}

/// A harness that sends Smart Auto flags as headers (Claude Code, opencode) with no model named or
/// inherited uses the tier the flags name, or Balance. Smart Auto flags reach the hub only on a
/// `hub/*` model, so a concrete model with them is an error.
fn header_transport_model(
    args: LaunchArgs,
    smart_auto: &SmartAutoFlags,
    inherited: Option<&str>,
) -> Result<LaunchArgs, String> {
    let named = args.model().or(inherited).map(str::to_owned);
    let hub_prefix = format!("{}/", HubSelector::NAMESPACE);
    match named {
        None => {
            let tier = smart_auto.tier().unwrap_or_default();
            Ok(with_model(
                args,
                format!("{}/{}", HubSelector::NAMESPACE, tier.selector()),
            ))
        }
        Some(_) if smart_auto.is_empty() => Ok(args),
        Some(model) if model.starts_with(&hub_prefix) => {
            LaunchCommand::check_tier_matches(&model, smart_auto)?;
            Ok(args)
        }
        Some(model) => Err(format!(
            "--smart-auto-* only applies to a {hub_prefix} model, and {model} is not one: name a {hub_prefix} model or drop the --smart-auto-* flags"
        )),
    }
}

/// Harnesses without header transport take Smart Auto flags only as a preset model id.
fn preset_args(args: LaunchArgs, smart_auto: &SmartAutoFlags) -> Result<LaunchArgs, String> {
    if smart_auto.is_empty() {
        return Ok(args);
    }
    if args.model().is_some() {
        return Err(
            "--smart-auto-* picks the model for this harness, so drop the model name".to_string(),
        );
    }
    Ok(with_model(args, smart_auto.preset_model()?))
}

/// The arguments with `model` first, so `LaunchArgs::parse` reads it as the model.
fn with_model(args: LaunchArgs, model: String) -> LaunchArgs {
    LaunchArgs::parse(
        std::iter::once(model)
            .chain(args.into_passthrough())
            .collect(),
    )
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
    use crate::schemas::smart_auto_tier::SmartAutoTier;

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

    fn flags_of(args: &[&str]) -> SmartAutoFlags {
        let mut flags = SmartAutoFlags::default();
        flags
            .take_flags(
                &mut args
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<String>>(),
            )
            .unwrap();
        flags
    }

    #[test]
    fn smart_auto_flags_are_taken_before_and_after_the_harness() {
        let command = parsed(&[
            "--smart-auto-tier=fast",
            "--profiles",
            "copilot",
            "claude",
            "--smart-auto-min-context",
            "1m",
            "m",
        ]);

        assert_eq!(command.harness, Harness::Claude);
        assert_eq!(command.profiles.to_list(), "copilot");
        assert_eq!(command.smart_auto.tier(), Some(SmartAutoTier::Fast));
        assert_eq!(command.args, ["m"]);
    }

    #[test]
    fn smart_auto_flags_without_a_harness_are_an_error() {
        assert!(parse(&["--smart-auto-tier=fast"]).is_err());
    }

    #[test]
    fn claude_without_a_model_defaults_to_smart_auto_balance() {
        let args = header_transport_model(
            LaunchArgs::from_strs(&["-p", "hi"]),
            &SmartAutoFlags::default(),
            None,
        )
        .unwrap();

        assert_eq!(args.model(), Some("hub/smart-auto-balance"));
        assert_eq!(args.into_passthrough(), ["-p", "hi"]);
    }

    #[test]
    fn a_tier_flag_or_an_inherited_model_changes_the_claude_default() {
        let efficiency = flags_of(&["--smart-auto-tier=efficiency"]);

        assert_eq!(
            header_transport_model(LaunchArgs::from_strs(&[]), &efficiency, None)
                .unwrap()
                .model(),
            Some("hub/smart-auto-efficiency")
        );
        assert_eq!(
            header_transport_model(
                LaunchArgs::from_strs(&[]),
                &SmartAutoFlags::default(),
                Some("hub/smart-auto-fast")
            )
            .unwrap()
            .model(),
            None
        );
        assert_eq!(
            header_transport_model(
                LaunchArgs::from_strs(&["copilot/m"]),
                &SmartAutoFlags::default(),
                None
            )
            .unwrap()
            .model(),
            Some("copilot/m")
        );
    }

    #[test]
    fn smart_auto_flags_next_to_a_concrete_model_are_an_error() {
        let fast = flags_of(&["--smart-auto-tier=fast"]);

        assert!(
            header_transport_model(LaunchArgs::from_strs(&["copilot/m"]), &fast, None).is_err()
        );
        assert!(
            header_transport_model(LaunchArgs::from_strs(&[]), &fast, Some("copilot/m")).is_err()
        );
        assert_eq!(
            header_transport_model(LaunchArgs::from_strs(&["hub/smart-auto-fast"]), &fast, None)
                .unwrap()
                .model(),
            Some("hub/smart-auto-fast")
        );
    }

    #[test]
    fn a_tier_flag_naming_another_tier_than_the_model_is_an_error() {
        let fast = flags_of(&["--smart-auto-tier=fast"]);

        let error = header_transport_model(
            LaunchArgs::from_strs(&["hub/smart-auto-balance"]),
            &fast,
            None,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "tier header `fast` conflicts with model `hub/smart-auto-balance`: use the matching model id or remove the header"
        );
        assert!(
            header_transport_model(
                LaunchArgs::from_strs(&[]),
                &fast,
                Some("hub/smart-auto-balance")
            )
            .is_err()
        );
        assert_eq!(
            header_transport_model(
                LaunchArgs::from_strs(&["hub/smart-auto-long-balance"]),
                &flags_of(&["--smart-auto-tier=balance"]),
                None
            )
            .unwrap()
            .model(),
            Some("hub/smart-auto-long-balance")
        );
    }

    #[test]
    fn harnesses_without_headers_take_flags_as_a_preset_model() {
        let flags = flags_of(&["--smart-auto-min-input=1m", "--smart-auto-tier=fast"]);

        let args = preset_args(LaunchArgs::from_strs(&["-p"]), &flags).unwrap();

        assert_eq!(args.model(), Some("hub/smart-auto-long-input-fast"));
        assert_eq!(args.into_passthrough(), ["-p"]);
        assert!(preset_args(LaunchArgs::from_strs(&["copilot/m"]), &flags).is_err());
        assert_eq!(
            preset_args(
                LaunchArgs::from_strs(&["copilot/m"]),
                &SmartAutoFlags::default()
            )
            .unwrap()
            .model(),
            Some("copilot/m")
        );
    }

    #[test]
    fn flags_a_preset_cannot_carry_fail_for_harnesses_without_headers() {
        let flags = flags_of(&["--smart-auto-role=planning"]);

        assert!(preset_args(LaunchArgs::from_strs(&[]), &flags).is_err());
    }
}
