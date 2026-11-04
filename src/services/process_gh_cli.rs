use std::io::ErrorKind;
use std::process::Stdio;
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use tokio::process::Command;

use crate::consts::GH_TOKEN_TIMEOUT;
use crate::services::gh_cli::{GhCli, GhOutcome};

const PUBLIC_GITHUB_HOST: &str = "github.com";
const PUBLIC_TOKEN_VAR: &str = "GH_TOKEN";
const ENTERPRISE_TOKEN_VAR: &str = "GH_ENTERPRISE_TOKEN";
const MAX_STDERR_CHARS: usize = 200;
const SPAWN_BUSY_ATTEMPTS: u32 = 5;
const SPAWN_BUSY_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Runs the real gh binary, without a shell. A token in the process
/// environment wins, the same way gh itself resolves one.
#[derive(Debug, Default)]
pub struct ProcessGhCli;

impl GhCli for ProcessGhCli {
    fn auth_token<'a>(
        &'a self,
        gh_path: &'a str,
        host: &'a str,
        user: Option<&'a str>,
    ) -> BoxFuture<'a, GhOutcome> {
        async move {
            if let Some(token) = env_token(host, |name| std::env::var(name).ok()) {
                return GhOutcome::Token(token);
            }
            run_gh(gh_path, host, user, GH_TOKEN_TIMEOUT).await
        }
        .boxed()
    }
}

fn env_token(host: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    let name = if host == PUBLIC_GITHUB_HOST {
        PUBLIC_TOKEN_VAR
    } else {
        ENTERPRISE_TOKEN_VAR
    };
    lookup(name).filter(|token| !token.is_empty())
}

async fn run_gh(gh_path: &str, host: &str, user: Option<&str>, limit: Duration) -> GhOutcome {
    let mut command = Command::new(gh_path);
    command.args(["auth", "token", "--hostname", host]);
    if let Some(user) = user {
        command.args(["--user", user]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match spawn_retrying_busy(&mut command).await {
        Ok(child) => child,
        Err(e) if e.kind() == ErrorKind::NotFound => return GhOutcome::NotInstalled,
        Err(e) => return GhOutcome::Failed(format!("{gh_path} could not start: {e}")),
    };
    let output = match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return GhOutcome::Failed(format!("{gh_path} could not be read: {e}")),
        Err(_) => {
            return GhOutcome::Failed(format!(
                "{gh_path} auth token did not finish within {} ms",
                limit.as_millis()
            ));
        }
    };
    if !output.status.success() {
        return GhOutcome::Failed(format!(
            "{gh_path} auth token exited with {}: {}",
            output.status,
            first_stderr_line(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let token = stdout.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return GhOutcome::Failed(format!("{gh_path} auth token printed no usable token"));
    }
    GhOutcome::Token(token.to_string())
}

/// Linux refuses to run a file another process still holds open for writing (ETXTBSY),
/// e.g. while `gh` is being replaced. That clears within milliseconds, so retry briefly.
async fn spawn_retrying_busy(command: &mut Command) -> std::io::Result<tokio::process::Child> {
    let mut attempt = 1;
    loop {
        match command.spawn() {
            Err(e)
                if e.kind() == ErrorKind::ExecutableFileBusy && attempt < SPAWN_BUSY_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(SPAWN_BUSY_RETRY_DELAY).await;
            }
            result => return result,
        }
    }
}

fn first_stderr_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(MAX_STDERR_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_from(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            vars.iter()
                .find(|(var, _)| var == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn env_token_uses_gh_token_for_github_com() {
        let lookup = lookup_from(&[(PUBLIC_TOKEN_VAR, "public"), (ENTERPRISE_TOKEN_VAR, "ghe")]);

        assert_eq!(env_token("github.com", &lookup).as_deref(), Some("public"));
    }

    #[test]
    fn env_token_uses_enterprise_token_for_other_hosts() {
        let lookup = lookup_from(&[(PUBLIC_TOKEN_VAR, "public"), (ENTERPRISE_TOKEN_VAR, "ghe")]);

        assert_eq!(
            env_token("ghe.example.com", &lookup).as_deref(),
            Some("ghe")
        );
    }

    #[test]
    fn env_token_ignores_empty_value() {
        let lookup = lookup_from(&[(PUBLIC_TOKEN_VAR, "")]);

        assert_eq!(env_token("github.com", &lookup), None);
    }

    #[test]
    fn first_stderr_line_truncates_long_line() {
        let stderr = format!("{}\nsecond line", "x".repeat(MAX_STDERR_CHARS + 50));

        assert_eq!(
            first_stderr_line(stderr.as_bytes()),
            "x".repeat(MAX_STDERR_CHARS)
        );
    }

    #[tokio::test]
    async fn run_gh_with_missing_binary_is_not_installed() {
        let missing = std::env::temp_dir().join("llm-hub-missing-gh").join("gh");

        let outcome = run_gh(
            missing.to_str().unwrap(),
            "github.com",
            None,
            GH_TOKEN_TIMEOUT,
        )
        .await;

        assert_eq!(outcome, GhOutcome::NotInstalled);
    }

    #[cfg(unix)]
    mod unix {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        use super::*;

        fn write_script(dir: &tempfile::TempDir, body: &str) -> PathBuf {
            let path = dir.path().join("gh");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        #[tokio::test]
        async fn run_gh_with_false_binary_fails() {
            let outcome = run_gh("/usr/bin/false", "github.com", None, GH_TOKEN_TIMEOUT).await;

            assert!(matches!(outcome, GhOutcome::Failed(_)), "{outcome:?}");
        }

        #[tokio::test]
        async fn run_gh_returns_trimmed_stdout() {
            let dir = tempfile::tempdir().unwrap();
            let gh = write_script(&dir, "printf '  gho_token\\n'");

            let outcome = run_gh(gh.to_str().unwrap(), "github.com", None, GH_TOKEN_TIMEOUT).await;

            assert_eq!(outcome, GhOutcome::Token("gho_token".to_string()));
        }

        #[tokio::test]
        async fn run_gh_failure_reports_fixed_argv_from_stderr() {
            let dir = tempfile::tempdir().unwrap();
            let gh = write_script(&dir, "echo \"$@\" >&2; exit 4");

            let outcome = run_gh(
                gh.to_str().unwrap(),
                "ghe.example.com",
                Some("octocat"),
                GH_TOKEN_TIMEOUT,
            )
            .await;

            let GhOutcome::Failed(reason) = outcome else {
                panic!("expected Failed, got {outcome:?}");
            };
            assert!(
                reason.ends_with(": auth token --hostname ghe.example.com --user octocat"),
                "{reason}"
            );
        }

        #[tokio::test]
        async fn run_gh_rejects_whitespace_stdout_without_echoing_it() {
            let dir = tempfile::tempdir().unwrap();
            let gh = write_script(&dir, "echo 'not a token'");

            let outcome = run_gh(gh.to_str().unwrap(), "github.com", None, GH_TOKEN_TIMEOUT).await;

            let GhOutcome::Failed(reason) = outcome else {
                panic!("expected Failed, got {outcome:?}");
            };
            assert!(!reason.contains("not a token"), "{reason}");
        }

        #[tokio::test]
        async fn run_gh_that_never_exits_times_out_and_is_killed() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let gh = write_script(
                &dir,
                &format!("echo $$ > {}; exec sleep 30", pid_file.display()),
            );

            let outcome = run_gh(
                gh.to_str().unwrap(),
                "github.com",
                None,
                Duration::from_secs(1),
            )
            .await;

            let GhOutcome::Failed(reason) = outcome else {
                panic!("expected Failed, got {outcome:?}");
            };
            assert!(reason.contains("did not finish within 1000 ms"), "{reason}");
            let pid = std::fs::read_to_string(&pid_file).unwrap();
            assert!(!process_alive(pid.trim()).await, "gh {pid} still running");
        }

        // kill_on_drop kills without waiting, so poll until the pid is gone or only a zombie.
        async fn process_alive(pid: &str) -> bool {
            for _ in 0..50 {
                let probe = std::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", pid])
                    .output()
                    .unwrap();
                let stat = String::from_utf8_lossy(&probe.stdout);
                if stat.trim().is_empty() || stat.trim_start().starts_with('Z') {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            true
        }
    }
}
