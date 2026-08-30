use std::path::Path;

use crate::consts::SERVICE_LABEL;

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
    let exe = xml_escape(exe);
    let cwd = xml_escape(cwd);
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
StandardOutput=append:{cwd}/logs/llm-hub.log
StandardError=append:{cwd}/logs/llm-hub.log
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
"#
    )
}

/// Task Scheduler XML has no environment element, so the action wraps the
/// binary through `cmd /c set LLM_HUB_SERVICE=1&& <exe>`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn render_windows_task_xml(exe: &Path, cwd: &Path) -> String {
    let exe = xml_escape(exe);
    let cwd = xml_escape(cwd);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Settings>
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
      <Command>cmd</Command>
      <Arguments>/c set LLM_HUB_SERVICE=1&amp;&amp; "{exe}" >> "{cwd}\logs\llm-hub.log" 2&gt;&amp;1</Arguments>
      <WorkingDirectory>{cwd}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn xml_escape(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
        run("launchctl", &["bootstrap", &domain, &plist_arg])?;
        println!("service {SERVICE_LABEL} installed and running");
        Ok(())
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
    use std::path::Path;

    use crate::consts::SERVICE_NAME;

    use super::{render_windows_task_xml, run};

    pub fn install(exe: &Path, cwd: &Path) -> Result<(), String> {
        let xml = std::env::temp_dir().join("llm-hub-task.xml");
        std::fs::write(&xml, render_windows_task_xml(exe, cwd)).map_err(|e| e.to_string())?;
        println!("wrote {}", xml.display());
        let xml_arg = xml.display().to_string();
        run(
            "schtasks",
            &["/Create", "/TN", SERVICE_NAME, "/XML", &xml_arg, "/F"],
        )
    }

    pub fn uninstall() -> Result<(), String> {
        run("schtasks", &["/Delete", "/TN", SERVICE_NAME, "/F"])
    }

    pub fn status() -> Result<(), String> {
        run("schtasks", &["/Query", "/TN", SERVICE_NAME])
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
        assert!(out.contains("Restart=always"));
        assert!(out.contains("StandardOutput=append:/srv/hub/logs/llm-hub.log"));
        assert!(out.contains("WantedBy=default.target"));
    }

    #[test]
    fn windows_task_xml_runs_exe_in_cwd_with_restart_on_failure() {
        let out = render_windows_task_xml(Path::new("C:/hub/llm-hub.exe"), Path::new("C:/hub"));

        assert!(out.contains("set LLM_HUB_SERVICE=1&amp;&amp; \"C:/hub/llm-hub.exe\""));
        assert!(out.contains("<WorkingDirectory>C:/hub</WorkingDirectory>"));
        assert!(out.contains("<LogonTrigger>"));
        assert!(out.contains("<RestartOnFailure>"));
        assert!(out.contains(r#">> "C:/hub\logs\llm-hub.log" 2&gt;&amp;1"#));
    }

    #[test]
    fn renderers_escape_xml_paths() {
        let out = render_launchd_plist(Path::new("/a&b/llm-hub"), Path::new("/a&b"));

        assert!(out.contains("<string>/a&amp;b/llm-hub</string>"));
        assert!(!out.contains("/a&b/"));
    }
}
