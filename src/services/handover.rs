//! `llm-hub service handover`: replaces the running service process with this binary and drops
//! no request. The new process binds the service port beside the old one (`SO_REUSEPORT`) and
//! must pass `/healthz`. Only then does the old process get SIGTERM: it stops accepting, finishes
//! its open requests and exits. A new process that fails leaves the old one serving.
//!
//! The new process is a bridge: the supervisor did not start it. When the old process exits, the
//! supervisor starts its own process, which takes the port over from the bridge the same way.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::consts::{SERVICE_BIND, SERVICE_PORT};
use crate::services::instance;
use crate::services::service_manager::{endpoint_healthy, health_client, service_pid};

/// How many `/healthz` probes the new process gets before the handover is cancelled.
const HEALTH_CHECKS: usize = 30;
/// The pause between two `/healthz` probes.
const HEALTH_DELAY: Duration = Duration::from_millis(500);
/// The `PATH` launchd gives a `LaunchAgent`, so the new process finds the same programs.
const SERVICE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// The variables the new process keeps from the caller. `TMPDIR` places the PID file.
const KEPT_VARS: [&str; 5] = ["HOME", "USER", "LOGNAME", "SHELL", "TMPDIR"];

/// The address a handover replaces and how it checks the new process.
#[derive(Debug)]
struct Target {
    bind: String,
    health_url: String,
    health_checks: usize,
    health_delay: Duration,
}

/// What `service handover` did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// No healthy hub served the port, so the service was installed (a plain restart).
    Installed,
    /// `new` serves the port; `old` drains its open requests and exits. `waiter` is a service
    /// process that waited beside a hub; it got SIGTERM so the supervisor starts a fresh one.
    HandedOver {
        old: u32,
        new: u32,
        waiter: Option<u32>,
    },
}

/// Hands the service port over from the running hub to this binary.
///
/// # Errors
/// Fails when the new process does not start or does not pass `/healthz`. The old process keeps
/// serving in that case.
pub async fn handover() -> Result<(), String> {
    let resolved = crate::utils::exe::current_exe_resolved().map_err(|e| e.to_string())?;
    let exe = crate::utils::exe::stable_service_exe(&resolved);
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let install = crate::services::service_manager::install;
    let outcome = handover_to(&service_target(), &exe, &cwd, install, service_pid).await?;
    println!("{}", report(&outcome));
    Ok(())
}

/// The installed service's address and the probes the new process gets.
fn service_target() -> Target {
    Target {
        bind: format!("{SERVICE_BIND}:{SERVICE_PORT}"),
        health_url: format!("http://127.0.0.1:{SERVICE_PORT}/healthz"),
        health_checks: HEALTH_CHECKS,
        health_delay: HEALTH_DELAY,
    }
}

fn report(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Installed => "no healthy llm-hub served the port; installed the service".into(),
        Outcome::HandedOver { old, new, waiter } => {
            let stopped = waiter.map_or_else(String::new, |pid| {
                format!("; stopped the waiting service process {pid}")
            });
            format!(
                "handed over to pid {new}; pid {old} finishes its open requests and exits{stopped}"
            )
        }
    }
}

async fn handover_to(
    target: &Target,
    exe: &Path,
    cwd: &Path,
    install: impl FnOnce() -> Result<(), String>,
    service_pid: impl FnOnce() -> Option<u32>,
) -> Result<Outcome, String> {
    let client = health_client()?;
    // A hub that answers /healthz wrote the PID file, so the PID names that hub.
    let serving = endpoint_healthy(&client, &target.health_url).await;
    let Some(old) = instance::live_server(&target.bind).filter(|_| serving) else {
        install()?;
        return Ok(Outcome::Installed);
    };
    let command = successor_command(exe, cwd, old, |key| std::env::var_os(key))?;
    let mut child = instance::start_successor(&target.bind, command).await?;
    let new = child.id();
    if !passes_health(target, &client, &mut child).await {
        instance::terminate(new);
        instance::record_pid(&target.bind, old)
            .map_err(|e| format!("cannot restore the pid file: {e}"))?;
        return Err(format!(
            "the new llm-hub (pid {new}) did not pass /healthz; pid {old} keeps serving"
        ));
    }
    // Connections in the old hub's accept queue are served before its listener closes.
    tokio::time::sleep(instance::SETTLE).await;
    instance::terminate(old);
    let waiter = stale_waiter(service_pid(), old, new);
    if let Some(waiter) = waiter {
        // The waiter only exits on a signal. Once the old hub is gone, the supervisor's fresh
        // process takes the port over from the new hub, which is a bridge.
        instance::wait_gone(old).await;
        instance::terminate(waiter);
    }
    Ok(Outcome::HandedOver { old, new, waiter })
}

/// The supervisor's process when it is neither hub: an earlier copy that waits beside a hub that
/// the supervisor did not start, and would wait for as long as any hub serves.
fn stale_waiter(service: Option<u32>, old: u32, new: u32) -> Option<u32> {
    service.filter(|pid| *pid != old && *pid != new && instance::pid_alive(*pid))
}

/// True when `/healthz` answers while the new process still runs.
async fn passes_health(target: &Target, client: &reqwest::Client, child: &mut Child) -> bool {
    for _ in 0..target.health_checks {
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        if endpoint_healthy(client, &target.health_url).await {
            return true;
        }
        tokio::time::sleep(target.health_delay).await;
    }
    false
}

/// The new service process: the service environment, the log file, the bridge flag and its own
/// process group, so the old process's supervisor does not stop it when the old process exits.
fn successor_command(
    exe: &Path,
    cwd: &Path,
    old: u32,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<Command, String> {
    let logs = cwd.join("logs");
    std::fs::create_dir_all(&logs).map_err(|e| format!("cannot create logs dir: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("llm-hub.log"))
        .map_err(|e| format!("cannot open the log file: {e}"))?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = Command::new(exe);
    command.current_dir(cwd).env_clear();
    for key in KEPT_VARS {
        if let Some(value) = env(key) {
            command.env(key, value);
        }
    }
    command
        .env("PATH", SERVICE_PATH)
        .env("LLM_HUB_SERVICE", "1")
        .env("LLM_HUB_PORT", SERVICE_PORT.to_string())
        .env("LLM_HUB_BIND", SERVICE_BIND)
        .env(instance::HANDOVER_VAR, old.to_string())
        .env(instance::BRIDGE_VAR, "1")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    Ok(command)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;

    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn target(server: &MockServer, checks: usize) -> Target {
        Target {
            bind: server.address().to_string(),
            health_url: format!("{}/healthz", server.uri()),
            health_checks: checks,
            health_delay: Duration::from_millis(10),
        }
    }

    /// A stand-in hub binary: it writes its PID to the PID file of `bind`, then sleeps.
    fn fake_hub(dir: &Path, bind: &str) -> std::path::PathBuf {
        let exe = dir.join("fake-hub");
        let script = format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            instance::pid_file(bind).display()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    /// A live process that plays the old hub, recorded in the PID file of `bind`.
    fn old_hub(bind: &str) -> Child {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        instance::record_pid(bind, child.id()).unwrap();
        child
    }

    async fn healthy_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(path("/healthz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_healthy_successor_takes_over_and_the_old_hub_gets_sigterm() {
        let server = healthy_server().await;
        let target = target(&server, 3);
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_hub(dir.path(), &target.bind);
        let mut old = old_hub(&target.bind);

        let outcome = handover_to(
            &target,
            &exe,
            dir.path(),
            || Err("no install".into()),
            || None,
        )
        .await
        .unwrap();

        let Outcome::HandedOver {
            old: old_pid,
            new,
            waiter,
        } = outcome
        else {
            panic!("expected a handover, got {outcome:?}");
        };
        assert_eq!(old_pid, old.id());
        assert_eq!(waiter, None);
        assert_eq!(instance::read_pid(&target.bind), Some(new));
        let status = old.wait().unwrap();
        assert!(!status.success(), "the old hub must stop on SIGTERM");
        instance::terminate(new);
        assert!(dir.path().join("logs/llm-hub.log").exists());
        let _ = std::fs::remove_file(instance::pid_file(&target.bind));
    }

    #[tokio::test]
    async fn a_service_process_that_waits_beside_the_hub_is_stopped_after_the_old_hub_exits() {
        let server = healthy_server().await;
        let target = target(&server, 3);
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_hub(dir.path(), &target.bind);
        let old = old_hub(&target.bind);
        let old_pid = old.id();
        // A reaper thread collects the old hub, so it does not linger as a zombie.
        let reaper = std::thread::spawn(move || {
            let mut old = old;
            old.wait()
        });
        let mut waiter = Command::new("sleep").arg("30").spawn().unwrap();
        let waiter_pid = waiter.id();

        let outcome = handover_to(
            &target,
            &exe,
            dir.path(),
            || Err("no install".into()),
            || Some(waiter_pid),
        )
        .await
        .unwrap();

        let Outcome::HandedOver {
            old,
            new,
            waiter: stopped,
        } = outcome
        else {
            panic!("expected a handover, got {outcome:?}");
        };
        assert_eq!((old, stopped), (old_pid, Some(waiter_pid)));
        assert!(!reaper.join().unwrap().unwrap().success());
        assert!(
            !waiter.wait().unwrap().success(),
            "the waiter must stop on SIGTERM"
        );
        instance::terminate(new);
        let _ = std::fs::remove_file(instance::pid_file(&target.bind));
    }

    #[test]
    fn only_a_live_service_process_that_is_neither_hub_is_a_stale_waiter() {
        let me = std::process::id();

        assert_eq!(stale_waiter(None, 1, 2), None);
        assert_eq!(stale_waiter(Some(me), me, 2), None);
        assert_eq!(stale_waiter(Some(me), 1, me), None);
        assert_eq!(stale_waiter(Some(999_999_999), 1, 2), None);
        assert_eq!(stale_waiter(Some(me), 1, 2), Some(me));
    }

    #[tokio::test]
    async fn a_successor_that_fails_healthz_is_stopped_and_the_old_hub_keeps_serving() {
        let server = MockServer::start().await;
        Mock::given(path("/healthz"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(path("/healthz"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let target = target(&server, 2);
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_hub(dir.path(), &target.bind);
        let mut old = old_hub(&target.bind);

        let error = handover_to(
            &target,
            &exe,
            dir.path(),
            || Err("no install".into()),
            || None,
        )
        .await
        .unwrap_err();

        assert!(error.contains("did not pass /healthz"), "{error}");
        assert_eq!(instance::read_pid(&target.bind), Some(old.id()));
        assert!(
            old.try_wait().unwrap().is_none(),
            "the old hub must keep serving"
        );
        old.kill().unwrap();
        old.wait().unwrap();
        let _ = std::fs::remove_file(instance::pid_file(&target.bind));
    }

    #[tokio::test]
    async fn a_successor_that_exits_fails_the_health_check() {
        let server = healthy_server().await;
        let target = target(&server, 2);
        let mut child = Command::new("true").spawn().unwrap();
        child.wait().unwrap();
        let client = health_client().unwrap();

        assert!(!passes_health(&target, &client, &mut child).await);
    }

    #[tokio::test]
    async fn no_healthy_hub_means_a_plain_install() {
        let server = MockServer::start().await;
        let target = target(&server, 1);
        let dir = tempfile::tempdir().unwrap();
        let mut installs = 0;

        let install = || -> Result<(), String> {
            installs += 1;
            Ok(())
        };

        let outcome = handover_to(
            &target,
            Path::new("/no/such/hub"),
            dir.path(),
            install,
            || None,
        )
        .await;

        assert_eq!(outcome, Ok(Outcome::Installed));
        assert_eq!(installs, 1);
    }

    #[tokio::test]
    async fn a_failed_install_is_reported() {
        let server = MockServer::start().await;
        let target = target(&server, 1);
        let dir = tempfile::tempdir().unwrap();

        let install =
            || -> Result<(), String> { Err("launchctl exited with exit status: 5".into()) };

        let outcome = handover_to(
            &target,
            Path::new("/no/such/hub"),
            dir.path(),
            install,
            || None,
        )
        .await;

        assert_eq!(outcome, Err("launchctl exited with exit status: 5".into()));
    }

    #[test]
    fn the_successor_runs_with_the_service_environment_only() {
        let dir = tempfile::tempdir().unwrap();
        let caller: HashMap<&str, &str> = [("HOME", "/home/me"), ("TMPDIR", "/tmp/me")].into();

        let command = successor_command(Path::new("/opt/llm-hub"), dir.path(), 77, |key| {
            caller.get(key).map(OsString::from)
        })
        .unwrap();

        let envs: HashMap<&OsStr, Option<&OsStr>> = command.get_envs().collect();
        let value = |key: &str| envs.get(OsStr::new(key)).copied().flatten();
        assert_eq!(command.get_program(), "/opt/llm-hub");
        assert_eq!(command.get_current_dir(), Some(dir.path()));
        assert_eq!(value("HOME"), Some(OsStr::new("/home/me")));
        assert_eq!(value("TMPDIR"), Some(OsStr::new("/tmp/me")));
        assert!(value("USER").is_none());
        assert_eq!(value("PATH"), Some(OsStr::new(SERVICE_PATH)));
        assert_eq!(value("LLM_HUB_SERVICE"), Some(OsStr::new("1")));
        assert_eq!(
            value("LLM_HUB_PORT"),
            Some(OsStr::new(&SERVICE_PORT.to_string()))
        );
        assert_eq!(value("LLM_HUB_BIND"), Some(OsStr::new(SERVICE_BIND)));
        assert_eq!(value(instance::HANDOVER_VAR), Some(OsStr::new("77")));
        assert_eq!(value(instance::BRIDGE_VAR), Some(OsStr::new("1")));
        assert!(dir.path().join("logs/llm-hub.log").exists());
    }

    #[test]
    fn the_service_target_is_the_installed_port() {
        let target = service_target();

        assert_eq!(target.bind, format!("{SERVICE_BIND}:{SERVICE_PORT}"));
        assert_eq!(
            target.health_url,
            format!("http://127.0.0.1:{SERVICE_PORT}/healthz")
        );
        assert_eq!(target.health_checks, HEALTH_CHECKS);
        assert_eq!(target.health_delay, HEALTH_DELAY);
    }

    #[test]
    fn the_report_names_both_processes_or_the_install() {
        assert_eq!(
            report(&Outcome::HandedOver {
                old: 1,
                new: 2,
                waiter: None
            }),
            "handed over to pid 2; pid 1 finishes its open requests and exits"
        );
        assert!(
            report(&Outcome::HandedOver {
                old: 1,
                new: 2,
                waiter: Some(3)
            })
            .ends_with("; stopped the waiting service process 3")
        );
        assert!(report(&Outcome::Installed).contains("installed the service"));
    }

    #[test]
    fn a_cwd_that_cannot_hold_logs_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();

        let error = successor_command(Path::new("/opt/llm-hub"), &file, 1, |_| None).unwrap_err();

        assert!(error.starts_with("cannot create logs dir:"), "{error}");
    }
}
