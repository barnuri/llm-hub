//! The command sink: runs `LLM_HUB_NOTIFY_COMMAND` with the notification JSON on stdin.

use std::time::Duration;

use crate::schemas::notification::Notification;
use crate::services::local_tuner::command;

/// Runs one fixed argv per notification, without a shell.
#[derive(Debug, Clone)]
pub struct CommandSink {
    argv: Vec<String>,
    timeout: Duration,
}

impl CommandSink {
    /// A sink that runs `argv` and kills it after `timeout`.
    #[must_use]
    pub fn new(argv: Vec<String>, timeout: Duration) -> CommandSink {
        CommandSink { argv, timeout }
    }

    /// Runs the command with `notification` as JSON on stdin.
    ///
    /// # Errors
    /// Returns the command's output tail when it fails to start, times out or exits non-zero.
    pub async fn send(&self, notification: &Notification) -> Result<(), String> {
        let json = serde_json::to_vec(notification)
            .map_err(|e| format!("cannot encode the notification: {e}"))?;
        let result = command::run(&self.argv, Some(json), self.timeout).await;
        if result.ok { Ok(()) } else { Err(result.tail) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::notifications::desktop::test_notification;

    #[tokio::test]
    async fn send_without_argv_fails() {
        let error = CommandSink::new(Vec::new(), Duration::from_secs(5))
            .send(&test_notification())
            .await
            .unwrap_err();

        assert_eq!(error, "no command is configured");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;

        fn shell(script: &str) -> Vec<String> {
            vec!["sh".into(), "-c".into(), script.into()]
        }

        #[tokio::test]
        async fn send_writes_the_notification_json_to_stdin() {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("out.json");
            let sink = CommandSink::new(
                shell(&format!("cat > '{}'", out.display())),
                Duration::from_secs(10),
            );
            let notification = test_notification();

            sink.send(&notification).await.unwrap();

            let written: Notification =
                serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
            assert_eq!(written, notification);
        }

        #[tokio::test]
        async fn send_reports_the_tail_of_a_failing_command() {
            let sink = CommandSink::new(
                shell("cat > /dev/null; echo nope >&2; exit 2"),
                Duration::from_secs(10),
            );

            let error = sink.send(&test_notification()).await.unwrap_err();

            assert!(error.starts_with("nope\nsh exited with"), "{error}");
        }
    }
}
