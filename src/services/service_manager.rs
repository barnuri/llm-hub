use std::path::{Path, PathBuf};

use crate::consts::{SERVICE_BIND, SERVICE_LABEL, SERVICE_NAME, SERVICE_PORT};

/// `ping -n 3` waits about two seconds, so a hub that keeps crashing does not spin.
const WINDOWS_RESTART_DELAY_PINGS: u32 = 3;

/// `llm-hub service install`: registers the hub as an always-on background
/// service for the current user — launchd `LaunchAgent` on macOS, systemd user
/// unit on Linux, Task Scheduler task on Windows. The definition captures the
/// current executable path and working directory (so `.env` is found) and
/// sets `LLM_HUB_SERVICE=1` so auto-update knows a supervisor will restart us.
pub fn install() -> Result<(), String> {
    let resolved = crate::utils::exe::current_exe_resolved().map_err(|e| e.to_string())?;
    let exe = crate::utils::exe::stable_service_exe(&resolved);
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(cwd.join("logs"))
        .map_err(|e| format!("cannot create logs dir: {e}"))?;
    platform::install(&exe, &cwd)
}

pub fn uninstall() -> Result<(), String> {
    platform::uninstall()
}

pub fn status() -> Result<(), String> {
    platform::status()
}

pub async fn ensure_installed_service() -> Result<bool, String> {
    if !platform::installed()? {
        return Ok(false);
    }
    let client = crate::services::http_client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .no_proxy()
        .build()
        .map_err(|error| error.to_string())?;
    let url = format!("http://127.0.0.1:{SERVICE_PORT}/healthz");
    ensure_healthy(
        || async { platform::running() && endpoint_healthy(&client, &url).await },
        platform::start,
        25,
        std::time::Duration::from_millis(200),
    )
    .await?;
    println!("llm-hub service is running at http://127.0.0.1:{SERVICE_PORT}");
    Ok(true)
}

async fn endpoint_healthy(client: &reqwest::Client, url: &str) -> bool {
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => {
            response.text().await.is_ok_and(|body| body == "ok")
        }
        _ => false,
    }
}

async fn ensure_healthy<F, Fut>(
    mut healthy: F,
    start: impl FnOnce() -> Result<(), String>,
    attempts: usize,
    delay: std::time::Duration,
) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if healthy().await {
        return Ok(());
    }
    start()?;
    for _ in 0..attempts {
        if healthy().await {
            return Ok(());
        }
        tokio::time::sleep(delay).await;
    }
    Err("service did not become healthy; check the service logs".to_string())
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Only the macOS service manager uses it")
)]
pub fn render_launchd_plist(exe: &Path, cwd: &Path) -> String {
    let exe = xml_escape(exe.display());
    let cwd = xml_escape(cwd.display());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{cwd}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>LLM_HUB_SERVICE</key>
        <string>1</string>
        <key>LLM_HUB_PORT</key>
        <string>{SERVICE_PORT}</string>
        <key>LLM_HUB_BIND</key>
        <string>{SERVICE_BIND}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{cwd}/logs/llm-hub.log</string>
    <key>StandardErrorPath</key>
    <string>{cwd}/logs/llm-hub.log</string>
</dict>
</plist>
"#
    )
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "Only the Linux service manager uses it")
)]
pub fn render_systemd_unit(exe: &Path, cwd: &Path) -> String {
    let exe = exe.display();
    let cwd = cwd.display();
    format!(
        r#"[Unit]
Description=llm-hub LLM proxy
After=network.target

[Service]
ExecStart="{exe}"
WorkingDirectory={cwd}
Environment=LLM_HUB_SERVICE=1
Environment=LLM_HUB_PORT={SERVICE_PORT}
Environment=LLM_HUB_BIND={SERVICE_BIND}
StandardOutput=append:{cwd}/logs/llm-hub.log
StandardError=append:{cwd}/logs/llm-hub.log
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
"#
    )
}

/// Writes the unit to `unit` and (re)starts it through `run`.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "Only the Linux service manager uses it")
)]
fn install_systemd_unit(
    unit: &Path,
    exe: &Path,
    cwd: &Path,
    mut run: impl FnMut(&str, &[&str]) -> Result<(), String>,
) -> Result<(), String> {
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(unit, render_systemd_unit(exe, cwd)).map_err(|e| e.to_string())?;
    println!("wrote {}", unit.display());
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", "--now", SERVICE_NAME])?;
    // enable --now is a no-op when already running — restart so a
    // reinstall always picks up the rewritten unit.
    run("systemctl", &["--user", "restart", SERVICE_NAME])?;
    println!(
        "hint: run `loginctl enable-linger $USER` so the service starts at boot without a login session"
    );
    Ok(())
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "Only the Linux service manager uses it")
)]
fn uninstall_systemd_unit(
    unit_path: impl FnOnce() -> Result<PathBuf, String>,
    mut run: impl FnMut(&str, &[&str]) -> Result<(), String>,
) -> Result<(), String> {
    if let Err(e) = run("systemctl", &["--user", "disable", "--now", SERVICE_NAME]) {
        println!("note: {e} (service may not be running)");
    }
    let unit = unit_path()?;
    if unit.exists() {
        std::fs::remove_file(&unit).map_err(|e| e.to_string())?;
        println!("removed {}", unit.display());
    }
    run("systemctl", &["--user", "daemon-reload"])
}

/// The macOS Application Firewall tool that `allow_through_firewall` drives.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Only the macOS service manager uses it")
)]
const FIREWALL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

/// Writes the plist to `plist` and (re)loads it into the `gui/<uid>` domain through `run`.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Only the macOS service manager uses it")
)]
fn install_launchd_agent(
    plist: &Path,
    uid: &str,
    exe: &Path,
    cwd: &Path,
    mut run: impl FnMut(&str, &[&str]) -> Result<(), String>,
    is_loaded: impl FnMut(&str) -> bool,
    firewall_on: bool,
) -> Result<(), String> {
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(plist, render_launchd_plist(exe, cwd)).map_err(|e| e.to_string())?;
    println!("wrote {}", plist.display());
    let domain = format!("gui/{uid}");
    let plist_arg = plist.display().to_string();
    // Reinstall-safe: unload any previous registration first (ignore
    // "not loaded"), so rerunning install applies the fresh definition.
    let target = format!("{domain}/{SERVICE_LABEL}");
    if run("launchctl", &["bootout", &target]).is_err() {
        println!("note: service was not previously loaded");
    }
    wait_until_unloaded(&target, is_loaded);
    run("launchctl", &["bootstrap", &domain, &plist_arg])?;
    if firewall_on {
        allow_through_firewall(exe, run);
    }
    println!("service {SERVICE_LABEL} installed and running");
    Ok(())
}

/// `bootout` returns before a graceful shutdown (stopping managed children) ends,
/// and `bootstrap` fails with status 5 while the old instance is still registered.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Only the macOS service manager uses it")
)]
fn wait_until_unloaded(target: &str, mut is_loaded: impl FnMut(&str) -> bool) {
    const POLL: std::time::Duration = std::time::Duration::from_millis(200);
    const MAX_POLLS: u32 = 100;
    for _ in 0..MAX_POLLS {
        if !is_loaded(target) {
            return;
        }
        std::thread::sleep(POLL);
    }
    println!("note: {target} still loaded after 20s; trying bootstrap anyway");
}

/// The macOS Application Firewall blocks inbound connections to unsigned
/// background binaries per-app (any port), so a LAN bind needs the binary
/// on its allow list. Best-effort: install still succeeds if this fails —
/// sudo prompts in the terminal, and on failure we print the manual fix.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Only the macOS service manager uses it")
)]
fn allow_through_firewall(exe: &Path, mut run: impl FnMut(&str, &[&str]) -> Result<(), String>) {
    let exe_arg = exe.display().to_string();
    println!("macOS firewall is on — allowing {exe_arg} (sudo may prompt)");
    let allowed = run("sudo", &[FIREWALL, "--add", &exe_arg])
        .and_then(|()| run("sudo", &[FIREWALL, "--unblockapp", &exe_arg]));
    if let Err(e) = allowed {
        println!("could not update the firewall ({e}); run manually:");
        println!("  sudo {FIREWALL} --add \"{exe_arg}\"");
        println!("  sudo {FIREWALL} --unblockapp \"{exe_arg}\"");
    }
}

#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Only the macOS service manager uses it")
)]
fn uninstall_launchd_agent(
    uid: &str,
    plist_path: impl FnOnce() -> Result<PathBuf, String>,
    mut run: impl FnMut(&str, &[&str]) -> Result<(), String>,
) -> Result<(), String> {
    let target = format!("gui/{uid}/{SERVICE_LABEL}");
    if let Err(e) = run("launchctl", &["bootout", &target]) {
        println!("note: {e} (service may not be running)");
    }
    let plist = plist_path()?;
    if plist.exists() {
        std::fs::remove_file(&plist).map_err(|e| e.to_string())?;
        println!("removed {}", plist.display());
    }
    Ok(())
}

/// The script a Windows logon task runs. It restarts the hub whenever it
/// exits, as launchd `KeepAlive` and systemd `Restart=always` do: Task
/// Scheduler only restarts a task that fails to start.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows service manager uses it")
)]
pub fn render_windows_launcher(exe: &Path, cwd: &Path) -> String {
    let exe = cmd_escape(exe);
    let cwd = cmd_escape(cwd);
    [
        "@echo off".to_string(),
        format!("cd /d \"{cwd}\""),
        "set LLM_HUB_SERVICE=1".to_string(),
        format!("set LLM_HUB_PORT={SERVICE_PORT}"),
        format!("set LLM_HUB_BIND={SERVICE_BIND}"),
        ":run".to_string(),
        format!("\"{exe}\" >> \"{cwd}\\logs\\llm-hub.log\" 2>&1"),
        format!("ping -n {WINDOWS_RESTART_DELAY_PINGS} 127.0.0.1 >nul"),
        "goto run".to_string(),
        String::new(),
    ]
    .join("\r\n")
}

/// A logon task for `user` alone, which a standard user may register without
/// admin rights. `StopExisting` makes `schtasks /Run` replace a running instance.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows service manager uses it")
)]
pub fn render_windows_task_xml(launcher: &Path, cwd: &Path, user: &str) -> String {
    let launcher = xml_escape(launcher.display());
    let cwd = xml_escape(cwd.display());
    let user = xml_escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>llm-hub LLM proxy</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>StopExisting</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>cmd.exe</Command>
      <Arguments>/d /c ""{launcher}""</Arguments>
      <WorkingDirectory>{cwd}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// `schtasks /XML` reads the encoding Task Scheduler exports: UTF-16LE with a BOM.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows service manager uses it")
)]
pub fn encode_utf16le_with_bom(text: &str) -> Vec<u8> {
    const BOM: u16 = 0xFEFF;
    std::iter::once(BOM)
        .chain(text.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn xml_escape(text: impl std::fmt::Display) -> String {
    text.to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A batch file expands `%NAME%` even inside quotes; `%%` is a literal `%`.
#[cfg_attr(
    not(windows),
    allow(dead_code, reason = "Only the Windows service manager uses it")
)]
fn cmd_escape(path: &Path) -> String {
    path.display().to_string().replace('%', "%%")
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    println!("running: {program} {}", args.join(" "));
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run {program}: {e}"))?;
    if status.success() {
        return Ok(());
    }
    Err(format!("{program} exited with {status}"))
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::{Path, PathBuf};

    use crate::consts::SERVICE_LABEL;

    use super::{FIREWALL, install_launchd_agent, run, uninstall_launchd_agent};

    pub fn install(exe: &Path, cwd: &Path) -> Result<(), String> {
        let firewall_on = firewall_enabled();
        install_launchd_agent(
            &plist_path()?,
            &uid()?,
            exe,
            cwd,
            run,
            is_loaded,
            firewall_on,
        )
    }

    pub fn uninstall() -> Result<(), String> {
        uninstall_launchd_agent(&uid()?, plist_path, run)
    }

    pub fn status() -> Result<(), String> {
        let target = format!("gui/{}/{SERVICE_LABEL}", uid()?);
        run("launchctl", &["print", &target])
    }

    pub fn installed() -> Result<bool, String> {
        plist_path()?
            .try_exists()
            .map_err(|error| error.to_string())
    }

    pub fn running() -> bool {
        let Ok(uid) = uid() else {
            return false;
        };
        let target = format!("gui/{uid}/{SERVICE_LABEL}");
        std::process::Command::new("launchctl")
            .args(["print", &target])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .any(|line| line.trim() == "state = running")
            })
    }

    pub fn start() -> Result<(), String> {
        let domain = format!("gui/{}", uid()?);
        let target = format!("{domain}/{SERVICE_LABEL}");
        if is_loaded(&target) {
            run("launchctl", &["kickstart", &target])
        } else {
            let plist = plist_path()?;
            run(
                "launchctl",
                &["bootstrap", &domain, &plist.display().to_string()],
            )
        }
    }

    fn is_loaded(target: &str) -> bool {
        std::process::Command::new("launchctl")
            .args(["print", target])
            .output()
            .is_ok_and(|out| out.status.success())
    }

    fn firewall_enabled() -> bool {
        std::process::Command::new(FIREWALL)
            .arg("--getglobalstate")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("enabled"))
    }

    pub(super) fn plist_path() -> Result<PathBuf, String> {
        let home = std::env::home_dir().ok_or("cannot determine home directory")?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist")))
    }

    pub(super) fn uid() -> Result<String, String> {
        let out = std::process::Command::new("id")
            .arg("-u")
            .output()
            .map_err(|e| format!("failed to run id -u: {e}"))?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::{Path, PathBuf};

    use crate::consts::SERVICE_NAME;

    use super::{install_systemd_unit, run, uninstall_systemd_unit};

    pub fn install(exe: &Path, cwd: &Path) -> Result<(), String> {
        install_systemd_unit(&unit_path()?, exe, cwd, run)
    }

    pub fn uninstall() -> Result<(), String> {
        uninstall_systemd_unit(unit_path, run)
    }

    pub fn status() -> Result<(), String> {
        run("systemctl", &["--user", "status", SERVICE_NAME])
    }

    pub fn installed() -> Result<bool, String> {
        unit_path()?.try_exists().map_err(|error| error.to_string())
    }

    pub fn running() -> bool {
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", SERVICE_NAME])
            .status()
            .is_ok_and(|status| status.success())
    }

    pub fn start() -> Result<(), String> {
        run("systemctl", &["--user", "start", SERVICE_NAME])
    }

    pub(super) fn unit_path() -> Result<PathBuf, String> {
        let home = std::env::home_dir().ok_or("cannot determine home directory")?;
        Ok(home
            .join(".config/systemd/user")
            .join(format!("{SERVICE_NAME}.service")))
    }
}

#[cfg(windows)]
mod platform {
    use std::path::{Path, PathBuf};

    use crate::consts::SERVICE_NAME;

    use super::{encode_utf16le_with_bom, render_windows_launcher, render_windows_task_xml, run};

    pub fn install(exe: &Path, cwd: &Path) -> Result<(), String> {
        let launcher = launcher_path()?;
        if let Some(dir) = launcher.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&launcher, render_windows_launcher(exe, cwd)).map_err(|e| e.to_string())?;
        println!("wrote {}", launcher.display());
        let task = launcher.with_extension("xml");
        let xml = render_windows_task_xml(&launcher, cwd, &current_user()?);
        std::fs::write(&task, encode_utf16le_with_bom(&xml)).map_err(|e| e.to_string())?;
        println!("wrote {}", task.display());
        let task_arg = task.display().to_string();
        run(
            "schtasks",
            &["/Create", "/TN", SERVICE_NAME, "/XML", &task_arg, "/F"],
        )?;
        run("schtasks", &["/Run", "/TN", SERVICE_NAME])?;
        println!("service {SERVICE_NAME} installed and running");
        Ok(())
    }

    pub fn uninstall() -> Result<(), String> {
        if let Err(e) = run("schtasks", &["/End", "/TN", SERVICE_NAME]) {
            println!("note: {e} (service may not be running)");
        }
        run("schtasks", &["/Delete", "/TN", SERVICE_NAME, "/F"])?;
        let launcher = launcher_path()?;
        for file in [launcher.with_extension("xml"), launcher] {
            if file.exists() {
                std::fs::remove_file(&file).map_err(|e| e.to_string())?;
                println!("removed {}", file.display());
            }
        }
        Ok(())
    }

    pub fn status() -> Result<(), String> {
        run(
            "schtasks",
            &["/Query", "/TN", SERVICE_NAME, "/V", "/FO", "LIST"],
        )
    }

    pub fn installed() -> Result<bool, String> {
        launcher_path()?
            .try_exists()
            .map_err(|error| error.to_string())
    }

    pub fn running() -> bool {
        let script = format!(
            "if ((Get-ScheduledTask -TaskName '{SERVICE_NAME}').State -eq 'Running') {{ exit 0 }} else {{ exit 1 }}"
        );
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .is_ok_and(|status| status.success())
    }

    pub fn start() -> Result<(), String> {
        if running() {
            return Ok(());
        }
        run("schtasks", &["/Run", "/TN", SERVICE_NAME])
    }

    fn launcher_path() -> Result<PathBuf, String> {
        let local_app_data = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
        Ok(PathBuf::from(local_app_data)
            .join(SERVICE_NAME)
            .join(format!("{SERVICE_NAME}-service.cmd")))
    }

    fn current_user() -> Result<String, String> {
        let user = std::env::var("USERNAME").map_err(|_| "USERNAME is not set")?;
        Ok(match std::env::var("USERDOMAIN") {
            Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
            _ => user,
        })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
    use std::path::Path;

    const UNSUPPORTED: &str = "service management is not supported on this platform";

    pub fn install(_exe: &Path, _cwd: &Path) -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    pub fn uninstall() -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    pub fn status() -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }

    pub fn installed() -> Result<bool, String> {
        Ok(false)
    }

    pub fn running() -> bool {
        false
    }

    pub fn start() -> Result<(), String> {
        Err(UNSUPPORTED.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_endpoint_requires_ok_response() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        let client = crate::services::http_client::client();
        assert!(endpoint_healthy(&client, &server.uri()).await);
    }

    #[tokio::test]
    async fn health_endpoint_rejects_errors_and_wrong_body() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

        let server = MockServer::start().await;
        Mock::given(path("/wrong"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not ok"))
            .mount(&server)
            .await;
        Mock::given(path("/error"))
            .respond_with(ResponseTemplate::new(503).set_body_string("ok"))
            .mount(&server)
            .await;
        let client = crate::services::http_client::client();
        assert!(!endpoint_healthy(&client, &format!("{}/wrong", server.uri())).await);
        assert!(!endpoint_healthy(&client, &format!("{}/error", server.uri())).await);
    }

    #[tokio::test]
    async fn healthy_service_is_not_restarted() {
        let result = ensure_healthy(
            || async { true },
            || Err("unexpected start".to_string()),
            1,
            std::time::Duration::ZERO,
        )
        .await;
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn stopped_service_is_started_and_checked() {
        let checks = std::cell::Cell::new(0);
        let starts = std::cell::Cell::new(0);
        let result = ensure_healthy(
            || {
                checks.set(checks.get() + 1);
                async { checks.get() > 1 }
            },
            || {
                starts.set(starts.get() + 1);
                Ok(())
            },
            2,
            std::time::Duration::ZERO,
        )
        .await;
        assert_eq!(result, Ok(()));
        assert_eq!(starts.get(), 1);
    }

    #[tokio::test]
    async fn service_start_error_is_reported() {
        let result = ensure_healthy(
            || async { false },
            || Err("start failed".to_string()),
            1,
            std::time::Duration::ZERO,
        )
        .await;
        assert_eq!(result, Err("start failed".to_string()));
    }

    #[tokio::test]
    async fn service_that_stays_unhealthy_returns_an_error() {
        let result =
            ensure_healthy(|| async { false }, || Ok(()), 1, std::time::Duration::ZERO).await;
        assert!(result.is_err());
    }

    #[test]
    fn launchd_plist_runs_exe_in_cwd_with_keepalive() {
        let out = render_launchd_plist(Path::new("/opt/bin/llm-hub"), Path::new("/srv/hub"));

        assert!(out.contains(&format!("<string>{SERVICE_LABEL}</string>")));
        assert!(out.contains("<string>/opt/bin/llm-hub</string>"));
        assert!(out.contains("<string>/srv/hub</string>"));
        assert!(out.contains("<key>LLM_HUB_SERVICE</key>"));
        assert!(out.contains(&format!("<string>{SERVICE_PORT}</string>")));
        assert!(out.contains(&format!("<string>{SERVICE_BIND}</string>")));
        assert!(out.contains("<key>RunAtLoad</key>"));
        assert!(out.contains("<key>KeepAlive</key>"));
        assert!(out.contains("<string>/srv/hub/logs/llm-hub.log</string>"));
    }

    #[test]
    fn systemd_unit_runs_exe_in_cwd_with_restart() {
        let out = render_systemd_unit(Path::new("/opt/bin/llm-hub"), Path::new("/srv/hub"));

        assert!(out.contains("ExecStart=\"/opt/bin/llm-hub\""));
        assert!(out.contains("WorkingDirectory=/srv/hub"));
        assert!(out.contains("Environment=LLM_HUB_SERVICE=1"));
        assert!(out.contains(&format!("Environment=LLM_HUB_PORT={SERVICE_PORT}")));
        assert!(out.contains(&format!("Environment=LLM_HUB_BIND={SERVICE_BIND}")));
        assert!(out.contains("Restart=always"));
        assert!(out.contains("StandardOutput=append:/srv/hub/logs/llm-hub.log"));
        assert!(out.contains("WantedBy=default.target"));
    }

    #[test]
    fn windows_launcher_restarts_exe_in_cwd_with_service_env() {
        let out = render_windows_launcher(
            Path::new(r"C:\hub\llm-hub.exe"),
            Path::new(r"C:\Users\me\100% hub"),
        );

        let lines: Vec<&str> = out.split("\r\n").collect();
        assert_eq!(
            lines,
            vec![
                "@echo off",
                r#"cd /d "C:\Users\me\100%% hub""#,
                "set LLM_HUB_SERVICE=1",
                &format!("set LLM_HUB_PORT={SERVICE_PORT}"),
                &format!("set LLM_HUB_BIND={SERVICE_BIND}"),
                ":run",
                r#""C:\hub\llm-hub.exe" >> "C:\Users\me\100%% hub\logs\llm-hub.log" 2>&1"#,
                "ping -n 3 127.0.0.1 >nul",
                "goto run",
                "",
            ]
        );
    }

    #[test]
    fn windows_task_xml_runs_launcher_at_users_logon() {
        let out = render_windows_task_xml(
            Path::new(r"C:\Users\me\AppData\Local\llm-hub\llm-hub-service.cmd"),
            Path::new(r"C:\hub"),
            r"DESKTOP\me",
        );

        assert!(out.contains(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        assert!(out.contains(
            r#"<Arguments>/d /c ""C:\Users\me\AppData\Local\llm-hub\llm-hub-service.cmd""</Arguments>"#
        ));
        assert!(out.contains(r"<WorkingDirectory>C:\hub</WorkingDirectory>"));
        assert_eq!(out.matches(r"<UserId>DESKTOP\me</UserId>").count(), 2);
        assert!(out.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(out.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(out.contains("<MultipleInstancesPolicy>StopExisting</MultipleInstancesPolicy>"));
        assert!(out.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    fn windows_task_xml_escapes_user_and_paths() {
        let out =
            render_windows_task_xml(Path::new(r"C:\a&b\l.cmd"), Path::new(r"C:\a&b"), "R&D\\me");

        assert!(out.contains(r#"""C:\a&amp;b\l.cmd"""#));
        assert!(out.contains(r"<UserId>R&amp;D\me</UserId>"));
        assert!(!out.contains("a&b"));
    }

    #[test]
    fn utf16le_encoding_starts_with_bom() {
        assert_eq!(
            encode_utf16le_with_bom("<a>é"),
            vec![0xFF, 0xFE, b'<', 0, b'a', 0, b'>', 0, 0xE9, 0]
        );
    }

    #[test]
    fn renderers_escape_xml_paths() {
        let out = render_launchd_plist(Path::new("/a&b/llm-hub"), Path::new("/a&b"));

        assert!(out.contains("<string>/a&amp;b/llm-hub</string>"));
        assert!(!out.contains("/a&b/"));
    }

    #[derive(Default)]
    struct FakeRunner {
        calls: Vec<String>,
        fail_on: Vec<&'static str>,
    }

    impl FakeRunner {
        fn failing_on(commands: &[&'static str]) -> Self {
            Self {
                calls: Vec::new(),
                fail_on: commands.to_vec(),
            }
        }

        fn run(&mut self, program: &str, args: &[&str]) -> Result<(), String> {
            let call = format!("{program} {}", args.join(" "));
            self.calls.push(call.clone());
            if self.fail_on.iter().any(|f| call.contains(f)) {
                return Err(format!("{program} exited with exit status: 1"));
            }
            Ok(())
        }
    }

    fn systemctl(args: &str) -> String {
        format!("systemctl --user {args}")
    }

    #[test]
    fn systemd_install_writes_the_unit_then_reloads_enables_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("systemd/user/llm-hub.service");
        let mut runner = FakeRunner::default();

        install_systemd_unit(
            &unit,
            Path::new("/opt/bin/llm-hub"),
            Path::new("/srv/hub"),
            |p, a| runner.run(p, a),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&unit).unwrap(),
            render_systemd_unit(Path::new("/opt/bin/llm-hub"), Path::new("/srv/hub"))
        );
        assert_eq!(
            runner.calls,
            vec![
                systemctl("daemon-reload"),
                systemctl(&format!("enable --now {SERVICE_NAME}")),
                systemctl(&format!("restart {SERVICE_NAME}")),
            ]
        );
    }

    #[test]
    fn systemd_install_overwrites_a_previous_unit() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("llm-hub.service");
        std::fs::write(&unit, "stale").unwrap();
        let mut runner = FakeRunner::default();

        install_systemd_unit(&unit, Path::new("/new/llm-hub"), dir.path(), |p, a| {
            runner.run(p, a)
        })
        .unwrap();

        let written = std::fs::read_to_string(&unit).unwrap();
        assert!(written.contains("ExecStart=\"/new/llm-hub\""));
        assert!(!written.contains("stale"));
    }

    #[test]
    fn systemd_install_stops_at_the_first_failed_command() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("llm-hub.service");
        let mut runner = FakeRunner::failing_on(&["enable"]);

        let err = install_systemd_unit(&unit, Path::new("/opt/llm-hub"), dir.path(), |p, a| {
            runner.run(p, a)
        })
        .unwrap_err();

        assert_eq!(err, "systemctl exited with exit status: 1");
        assert_eq!(
            runner.calls,
            vec![
                systemctl("daemon-reload"),
                systemctl(&format!("enable --now {SERVICE_NAME}")),
            ]
        );
    }

    #[test]
    fn systemd_install_fails_without_running_systemctl_when_the_unit_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        let mut runner = FakeRunner::default();

        let result = install_systemd_unit(
            &blocker.join("llm-hub.service"),
            Path::new("/opt/llm-hub"),
            dir.path(),
            |p, a| runner.run(p, a),
        );

        assert!(result.is_err());
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn systemd_uninstall_disables_removes_the_unit_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("llm-hub.service");
        std::fs::write(&unit, "unit").unwrap();
        let mut runner = FakeRunner::default();

        uninstall_systemd_unit(|| Ok(unit.clone()), |p, a| runner.run(p, a)).unwrap();

        assert!(!unit.exists());
        assert_eq!(
            runner.calls,
            vec![
                systemctl(&format!("disable --now {SERVICE_NAME}")),
                systemctl("daemon-reload"),
            ]
        );
    }

    #[test]
    fn systemd_uninstall_tolerates_a_stopped_service_and_a_missing_unit() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("llm-hub.service");
        let mut runner = FakeRunner::failing_on(&["disable"]);

        let result = uninstall_systemd_unit(|| Ok(unit.clone()), |p, a| runner.run(p, a));

        assert_eq!(result, Ok(()));
        assert_eq!(runner.calls.last(), Some(&systemctl("daemon-reload")));
    }

    #[test]
    fn systemd_uninstall_reports_a_failed_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = FakeRunner::failing_on(&["daemon-reload"]);

        let result = uninstall_systemd_unit(
            || Ok(dir.path().join("llm-hub.service")),
            |p, a| runner.run(p, a),
        );

        assert_eq!(result, Err("systemctl exited with exit status: 1".into()));
    }

    #[test]
    fn systemd_uninstall_fails_when_the_unit_path_is_unknown() {
        let mut runner = FakeRunner::default();

        let result = uninstall_systemd_unit(
            || Err("cannot determine home directory".to_string()),
            |p, a| runner.run(p, a),
        );

        assert_eq!(result, Err("cannot determine home directory".into()));
        assert_eq!(
            runner.calls,
            vec![systemctl(&format!("disable --now {SERVICE_NAME}"))]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_unit_lives_in_the_user_unit_dir() {
        let unit = platform::unit_path().unwrap();

        assert!(unit.ends_with(format!(".config/systemd/user/{SERVICE_NAME}.service")));
    }

    fn launchctl(args: &str) -> String {
        format!("launchctl {args}")
    }

    fn sudo_firewall(args: &str) -> String {
        format!("sudo {FIREWALL} {args}")
    }

    #[test]
    fn launchd_install_writes_the_plist_then_reloads_it_in_the_user_domain() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("LaunchAgents/llm-hub.plist");
        let mut runner = FakeRunner::default();

        install_launchd_agent(
            &plist,
            "501",
            Path::new("/opt/bin/llm-hub"),
            Path::new("/srv/hub"),
            |p, a| runner.run(p, a),
            |_| false,
            false,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&plist).unwrap(),
            render_launchd_plist(Path::new("/opt/bin/llm-hub"), Path::new("/srv/hub"))
        );
        assert_eq!(
            runner.calls,
            vec![
                launchctl(&format!("bootout gui/501/{SERVICE_LABEL}")),
                launchctl(&format!("bootstrap gui/501 {}", plist.display())),
            ]
        );
    }

    #[test]
    fn launchd_install_waits_for_the_old_instance_and_tolerates_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("llm-hub.plist");
        let mut runner = FakeRunner::failing_on(&["bootout"]);
        let mut probes = 0;

        install_launchd_agent(
            &plist,
            "501",
            Path::new("/opt/llm-hub"),
            dir.path(),
            |p, a| runner.run(p, a),
            |_| {
                probes += 1;
                probes < 2
            },
            false,
        )
        .unwrap();

        assert_eq!(probes, 2);
        assert_eq!(runner.calls.len(), 2);
    }

    #[test]
    fn launchd_install_reports_a_failed_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = FakeRunner::failing_on(&["bootstrap"]);

        let result = install_launchd_agent(
            &dir.path().join("llm-hub.plist"),
            "501",
            Path::new("/opt/llm-hub"),
            dir.path(),
            |p, a| runner.run(p, a),
            |_| false,
            true,
        );

        assert_eq!(result, Err("launchctl exited with exit status: 1".into()));
        assert!(!runner.calls.iter().any(|c| c.starts_with("sudo")));
    }

    #[test]
    fn launchd_install_fails_without_running_launchctl_when_the_plist_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "").unwrap();
        let mut runner = FakeRunner::default();

        let result = install_launchd_agent(
            &blocker.join("llm-hub.plist"),
            "501",
            Path::new("/opt/llm-hub"),
            dir.path(),
            |p, a| runner.run(p, a),
            |_| false,
            false,
        );

        assert!(result.is_err());
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn launchd_install_allows_the_binary_through_an_enabled_firewall() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = FakeRunner::default();

        install_launchd_agent(
            &dir.path().join("llm-hub.plist"),
            "501",
            Path::new("/opt/llm-hub"),
            dir.path(),
            |p, a| runner.run(p, a),
            |_| false,
            true,
        )
        .unwrap();

        assert_eq!(
            runner.calls[2..],
            [
                sudo_firewall("--add /opt/llm-hub"),
                sudo_firewall("--unblockapp /opt/llm-hub"),
            ]
        );
    }

    #[test]
    fn launchd_install_succeeds_when_the_firewall_update_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = FakeRunner::failing_on(&["--add"]);

        let result = install_launchd_agent(
            &dir.path().join("llm-hub.plist"),
            "501",
            Path::new("/opt/llm-hub"),
            dir.path(),
            |p, a| runner.run(p, a),
            |_| false,
            true,
        );

        assert_eq!(result, Ok(()));
        assert_eq!(
            runner.calls.last(),
            Some(&sudo_firewall("--add /opt/llm-hub"))
        );
    }

    #[test]
    fn launchd_uninstall_boots_out_and_removes_the_plist() {
        let dir = tempfile::tempdir().unwrap();
        let plist = dir.path().join("llm-hub.plist");
        std::fs::write(&plist, "plist").unwrap();
        let mut runner = FakeRunner::default();

        uninstall_launchd_agent("501", || Ok(plist.clone()), |p, a| runner.run(p, a)).unwrap();

        assert!(!plist.exists());
        assert_eq!(
            runner.calls,
            vec![launchctl(&format!("bootout gui/501/{SERVICE_LABEL}"))]
        );
    }

    #[test]
    fn launchd_uninstall_tolerates_a_stopped_service_and_a_missing_plist() {
        let dir = tempfile::tempdir().unwrap();
        let mut runner = FakeRunner::failing_on(&["bootout"]);

        let result = uninstall_launchd_agent(
            "501",
            || Ok(dir.path().join("llm-hub.plist")),
            |p, a| runner.run(p, a),
        );

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn launchd_uninstall_fails_when_the_plist_path_is_unknown() {
        let mut runner = FakeRunner::default();

        let result = uninstall_launchd_agent(
            "501",
            || Err("cannot determine home directory".to_string()),
            |p, a| runner.run(p, a),
        );

        assert_eq!(result, Err("cannot determine home directory".into()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_lives_in_the_user_launch_agents_dir() {
        let plist = platform::plist_path().unwrap();

        assert!(plist.ends_with(format!("Library/LaunchAgents/{SERVICE_LABEL}.plist")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn uid_is_the_numeric_user_id() {
        let uid = platform::uid().unwrap();

        assert!(
            !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()),
            "{uid}"
        );
    }

    #[test]
    fn xml_escape_replaces_markup_characters() {
        assert_eq!(xml_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn run_reports_a_program_that_cannot_start() {
        let err = run("llm-hub-test-no-such-program", &["--version"]).unwrap_err();

        assert!(
            err.starts_with("failed to run llm-hub-test-no-such-program: "),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_succeeds_when_the_program_exits_zero() {
        assert_eq!(run("sh", &["-c", "exit 0"]), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn run_reports_a_nonzero_exit_status() {
        let err = run("sh", &["-c", "exit 3"]).unwrap_err();

        assert!(err.starts_with("sh exited with "), "{err}");
        assert!(err.contains('3'), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn run_reports_a_nonzero_exit_status() {
        let err = run("cmd", &["/C", "exit 3"]).unwrap_err();

        assert!(err.starts_with("cmd exited with "), "{err}");
        assert!(err.contains('3'), "{err}");
    }
}
