pub mod claude_launcher;
pub mod codex_launcher;
pub mod copilot_cli_launcher;
pub mod harness;
pub mod hub_client;
pub mod hub_endpoint;
pub mod hub_model;
pub mod launch_args;
pub mod launch_spec;
pub mod opencode_launcher;
pub mod pi_launcher;

use self::claude_launcher::ClaudeLauncher;
use self::codex_launcher::CodexLauncher;
use self::copilot_cli_launcher::CopilotCliLauncher;
use self::harness::Harness;
use self::hub_client::HubClient;
use self::hub_endpoint::HubEndpoint;
use self::hub_model::HubModel;
use self::launch_args::LaunchArgs;
use self::launch_spec::LaunchSpec;
use self::opencode_launcher::OpencodeLauncher;
use self::pi_launcher::PiLauncher;

/// Checks that the hub answers, then replaces this process with the harness.
///
/// # Errors
/// Returns a message when the hub is unreachable, the pi extension cannot be
/// written, or the harness binary cannot start.
pub async fn run(harness: Harness, args: Vec<String>) -> Result<(), String> {
    let endpoint = HubEndpoint::from_lookup(|name| std::env::var(name).ok());
    let client = HubClient::new(endpoint)?;
    client.check_reachable().await?;
    let spec = build_spec(harness, &client, LaunchArgs::parse(args)).await?;
    spec.exec()
}

async fn build_spec(
    harness: Harness,
    client: &HubClient,
    args: LaunchArgs,
) -> Result<LaunchSpec, String> {
    let endpoint = client.endpoint();
    let spec = match harness {
        Harness::Claude => ClaudeLauncher::spec(endpoint, args, client.claude_settings().await),
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
            PiLauncher::spec(endpoint, args, &extension)
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
