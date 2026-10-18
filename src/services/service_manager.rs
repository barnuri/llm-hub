use std::path::Path;

use crate::consts::{SERVICE_BIND, SERVICE_LABEL, SERVICE_PORT};

/// `ping -n 3` waits about two seconds, so a hub that keeps crashing does not spin.
const WINDOWS_RESTART_DELAY_PINGS: u32 = 3;

/// `llm-hub service install`: registers the hub as an always-on background
/// service for the current user — launchd `LaunchAgent` on macOS, systemd user
/// unit on Linux, Task Scheduler task on Windows. The definition captures the
/// current executable path and working directory (so `.env` is found) and
/// sets `LLM_HUB_SERVICE=1` so auto-update knows a supervisor will restart us.
pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
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

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

/// The script a Windows logon task runs. It restarts the hub whenever it
/// exits, as launchd `KeepAlive` and systemd `Restart=always` do: Task
/// Scheduler only restarts a task that fails to start.
#[cfg_attr(not(windows), allow(dead_code))]
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
#[cfg_attr(not(windows), allow(dead_code))]
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
#[cfg_attr(not(windows), allow(dead_code))]
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
#[cfg_attr(not(windows), allow(dead_code))]
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

    use super::{render_launchd_plist, run};

    pub fn install(exe: &Path, cwd: &Path) -> Result<(), String> {
        let plist = plist_path()?;
        if let Some(dir) = plist.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&plist, render_launchd_plist(exe, cwd)).map_err(|e| e.to_string())?;
        println!("wrote {}", plist.display());
        let domain = format!("gui/{}", uid()?);
        let plist_arg = plist.display().to_string();
        // Reinstall-safe: unload any previous registration first (ignore
        // "not loaded"), so rerunning install applies the fresh definition.
        let target = format!("{domain}/{SERVICE_LABEL}");
        if run("launchctl", &["bootout", &target]).is_err() {
            println!("note: service was not previously loaded");
        }
        wait_until_unloaded(&target);
        run("launchctl", &["bootstrap", &domain, &plist_arg])?;
        allow_through_firewall(exe);
        println!("service {SERVICE_LABEL} installed and running");
        Ok(())
    }

    /// `bootout` returns before a graceful shutdown (stopping managed children) ends,
    /// and `bootstrap` fails with status 5 while the old instance is still registered.
    fn wait_until_unloaded(target: &str) {
        const POLL: std::time::Duration = std::time::Duration::from_millis(200);
        const MAX_POLLS: u32 = 100;
        for _ in 0..MAX_POLLS {
            let loaded = std::process::Command::new("launchctl")
                .args(["print", target])
                .output()
                .is_ok_and(|out| out.status.success());
            if !loaded {
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
    fn allow_through_firewall(exe: &Path) {
        const FIREWALL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
        let enabled = std::process::Command::new(FIREWALL)
            .arg("--getglobalstate")
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("enabled"));
        if !enabled {
            return;
        }
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

    pub fn uninstall() -> Result<(), String> {
        let target = format!("gui/{}/{SERVICE_LABEL}", uid()?);
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

    pub fn status() -> Result<(), String> {
        let target = format!("gui/{}/{SERVICE_LABEL}", uid()?);
        run("launchctl", &["print", &target])
    }

    fn plist_path() -> Result<PathBuf, String> {
        let home = std::env::home_dir().ok_or("cannot determine home directory")?;
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist")))
    }

    fn uid() -> Result<String, String> {
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

    use super::{render_systemd_unit, run};

    pub fn install(exe: &Path, cwd: &Path) -> Result<(), String> {
        let unit = unit_path()?;
        if let Some(dir) = unit.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&unit, render_systemd_unit(exe, cwd)).map_err(|e| e.to_string())?;
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

    pub fn uninstall() -> Result<(), String> {
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

    pub fn status() -> Result<(), String> {
        run("systemctl", &["--user", "status", SERVICE_NAME])
    }

    fn unit_path() -> Result<PathBuf, String> {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
