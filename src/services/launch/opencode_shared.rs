use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The shared opencode service: the one background server every opencode TUI can attach to.
/// It reads provider config from its own files, so a launcher can use it only when it already
/// serves a provider that points at this hub.
#[derive(Debug)]
pub struct OpencodeShared;

impl OpencodeShared {
    const TIMEOUT: Duration = Duration::from_secs(3);
    const POLL: Duration = Duration::from_millis(20);

    /// The id of an enabled provider on the shared service whose `baseURL` is `v1_url`.
    /// `None` when `program api get /api/provider` fails, times out or lists no such provider.
    #[must_use]
    pub fn provider_for(program: &str, v1_url: &str) -> Option<String> {
        let body = Self::query(program)?;
        Self::pick_provider(&body, v1_url)
    }

    fn query(program: &str) -> Option<String> {
        let mut child = Command::new(program)
            .args(["api", "get", "/api/provider"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Self::TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().ok()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(Self::POLL);
        };
        if !status.success() {
            return None;
        }
        let mut body = String::new();
        child.stdout.take()?.read_to_string(&mut body).ok()?;
        Some(body)
    }

    fn pick_provider(body: &str, v1_url: &str) -> Option<String> {
        let parsed: Value = serde_json::from_str(body).ok()?;
        let wanted = Self::normalize(v1_url);
        parsed
            .get("data")?
            .as_array()?
            .iter()
            .find(|provider| {
                provider.get("activation").and_then(Value::as_str) == Some("enabled")
                    && provider
                        .pointer("/settings/baseURL")
                        .and_then(Value::as_str)
                        .is_some_and(|url| Self::normalize(url) == wanted)
            })?
            .get("id")?
            .as_str()
            .map(str::to_owned)
    }

    /// Loopback spellings are one host, and a trailing slash does not matter.
    fn normalize(url: &str) -> String {
        let mut url = url.trim_end_matches('/').to_string();
        for host in ["127.0.0.1", "0.0.0.0", "[::1]"] {
            url = url.replace(host, "localhost");
        }
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const BODY: &str = r#"{"data":[
        {"id":"other","activation":"enabled","settings":{"baseURL":"http://x/v1"}},
        {"id":"off","activation":"disabled","settings":{"baseURL":"http://localhost:8888/v1"}},
        {"id":"llm-hub","activation":"enabled","settings":{"baseURL":"http://127.0.0.1:8888/v1/"}}]}"#;

    #[cfg(unix)]
    fn fake(script: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let program = path.to_string_lossy().to_string();
        (dir, program)
    }

    #[test]
    fn picks_the_enabled_provider_on_this_hub() {
        assert_eq!(
            OpencodeShared::pick_provider(BODY, "http://localhost:8888/v1"),
            Some("llm-hub".to_string())
        );
    }

    #[test]
    fn no_match_for_another_hub_or_bad_json() {
        assert_eq!(OpencodeShared::pick_provider(BODY, "http://h:1/v1"), None);
        assert_eq!(OpencodeShared::pick_provider("nope", "http://h:1/v1"), None);
        assert_eq!(OpencodeShared::pick_provider("{}", "http://h:1/v1"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_healthy_service_gives_the_provider_id() {
        let (_dir, program) = fake(&format!(
            "[ \"$1 $2 $3\" = \"api get /api/provider\" ] || exit 2\ncat <<'EOF'\n{BODY}\nEOF"
        ));

        assert_eq!(
            OpencodeShared::provider_for(&program, "http://localhost:8888/v1"),
            Some("llm-hub".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_service_gives_none() {
        let (_dir, program) = fake("exit 1");

        assert_eq!(
            OpencodeShared::provider_for(&program, "http://localhost:8888/v1"),
            None
        );
    }

    #[test]
    fn a_missing_program_gives_none() {
        assert_eq!(
            OpencodeShared::provider_for("/nonexistent/opencode", "http://localhost:8888/v1"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_service_times_out() {
        let (_dir, program) = fake("sleep 30");
        let start = Instant::now();

        assert_eq!(
            OpencodeShared::provider_for(&program, "http://localhost:8888/v1"),
            None
        );
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
