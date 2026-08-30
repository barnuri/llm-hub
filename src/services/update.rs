use serde::Deserialize;

use crate::consts::{GITHUB_REPO, VERSION};

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// `llm-hub update`: replace the running binary with the latest GitHub release.
pub async fn self_update() -> Result<(), String> {
    let release = fetch_latest().await?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    if latest == VERSION {
        println!("already up to date (v{VERSION})");
        return Ok(());
    }
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == expected_asset_name())
        .ok_or_else(|| {
            format!(
                "no release asset named {} in v{latest}",
                expected_asset_name()
            )
        })?;

    println!("updating v{VERSION} -> v{latest} ...");
    let bytes = http_client()?
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;

    replace_current_binary(&bytes)?;
    println!("updated to v{latest} — restart llm-hub to use it");
    Ok(())
}

/// Startup check: logs when a newer release exists. Never blocks serving.
pub fn spawn_update_check() {
    tokio::spawn(async {
        match fetch_latest().await {
            Ok(release) => {
                let latest = release.tag_name.trim_start_matches('v');
                if latest != VERSION {
                    tracing::info!(
                        "new version available: v{latest} (running v{VERSION}) — run `llm-hub update`"
                    );
                }
            }
            Err(e) => tracing::debug!("update check failed: {e}"),
        }
    });
}

async fn fetch_latest() -> Result<Release, String> {
    http_client()?
        .get(format!(
            "https://api.github.com/repos/{GITHUB_REPO}/releases/latest"
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json::<Release>()
        .await
        .map_err(|e| e.to_string())
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(format!("llm-hub/{VERSION}"))
        .build()
        .map_err(|e| e.to_string())
}

fn expected_asset_name() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!("llm-hub-{os}-{arch}")
}

/// Writes the new binary next to the current one, then atomically renames
/// over it. The running process keeps executing the old inode.
fn replace_current_binary(bytes: &[u8]) -> Result<(), String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let tmp = current.with_extension("update-tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("write update failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod failed: {e}"))?;
    }
    std::fs::rename(&tmp, &current).map_err(|e| format!("replace binary failed: {e}"))
}
