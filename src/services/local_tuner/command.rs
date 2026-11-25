//! Apply-command runner with a timeout and an output tail.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::services::process_gh_cli::spawn_retrying_busy;

const TAIL_CHARS: usize = 2_000;
// Four bytes per char covers any UTF-8 text, plus one char for a split boundary.
const TAIL_BYTES: usize = (TAIL_CHARS + 1) * 4;
const READ_CHUNK_BYTES: usize = 4_096;

/// The outcome of one command run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// True when the command started, finished within the timeout and exited with status 0.
    pub ok: bool,
    /// The last 2,000 characters of stdout and stderr in arrival order, followed by a
    /// status line when the run failed.
    pub tail: String,
}

/// Runs `argv` without a shell. It writes `stdin` to the child when given, kills the child
/// at `timeout`, and never fails: a start error or a timeout gives `ok: false`.
pub async fn run(argv: &[String], stdin: Option<Vec<u8>>, timeout: Duration) -> CommandResult {
    let Some((program, arguments)) = argv.split_first() else {
        return CommandResult {
            ok: false,
            tail: "no command is configured".to_string(),
        };
    };
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match spawn_retrying_busy(&mut command).await {
        Ok(child) => child,
        Err(e) => {
            return CommandResult {
                ok: false,
                tail: format!("{program} could not start: {e}"),
            };
        }
    };

    let mut tail = TailBuffer::default();
    let outcome = tokio::time::timeout(timeout, drive(&mut child, stdin, &mut tail)).await;
    let ok = match outcome {
        Ok(Ok(status)) if status.success() => true,
        Ok(Ok(status)) => {
            tail.push_line(&format!("{program} exited with {status}"));
            false
        }
        Ok(Err(e)) => {
            tail.push_line(&format!("{program} could not be awaited: {e}"));
            false
        }
        Err(_) => {
            // The child may already be gone, so a kill error carries no information.
            let _ = child.kill().await;
            tail.push_line(&format!(
                "{program} did not finish within {} ms and was killed",
                timeout.as_millis()
            ));
            false
        }
    };
    CommandResult {
        ok,
        tail: tail.render(),
    }
}

async fn drive(
    child: &mut Child,
    stdin: Option<Vec<u8>>,
    tail: &mut TailBuffer,
) -> std::io::Result<ExitStatus> {
    let writer = child.stdin.take();
    let feed = async move {
        let (Some(mut writer), Some(bytes)) = (writer, stdin) else {
            return;
        };
        // A child that exits without reading its input closes the pipe; that is not an error.
        let _ = writer.write_all(&bytes).await;
    };
    let collect = collect_output(child.stdout.take(), child.stderr.take(), tail);
    tokio::join!(feed, collect);
    child.wait().await
}

async fn collect_output(
    mut stdout: Option<impl AsyncRead + Unpin>,
    mut stderr: Option<impl AsyncRead + Unpin>,
    tail: &mut TailBuffer,
) {
    let mut stdout_chunk = [0u8; READ_CHUNK_BYTES];
    let mut stderr_chunk = [0u8; READ_CHUNK_BYTES];
    while stdout.is_some() || stderr.is_some() {
        tokio::select! {
            read = read_some(&mut stdout, &mut stdout_chunk), if stdout.is_some() => {
                match read {
                    Ok(n) if n > 0 => tail.push(&stdout_chunk[..n]),
                    _ => stdout = None,
                }
            }
            read = read_some(&mut stderr, &mut stderr_chunk), if stderr.is_some() => {
                match read {
                    Ok(n) if n > 0 => tail.push(&stderr_chunk[..n]),
                    _ => stderr = None,
                }
            }
        }
    }
}

async fn read_some(
    reader: &mut Option<impl AsyncRead + Unpin>,
    chunk: &mut [u8],
) -> std::io::Result<usize> {
    match reader {
        Some(reader) => reader.read(chunk).await,
        None => Ok(0),
    }
}

/// Keeps only the newest bytes, so a chatty command cannot grow memory without bound.
#[derive(Debug, Default)]
struct TailBuffer {
    bytes: Vec<u8>,
}

impl TailBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > TAIL_BYTES * 2 {
            let excess = self.bytes.len() - TAIL_BYTES;
            self.bytes.drain(..excess);
        }
    }

    fn push_line(&mut self, line: &str) {
        if !self.bytes.is_empty() && !self.bytes.ends_with(b"\n") {
            self.push(b"\n");
        }
        self.push(line.as_bytes());
    }

    fn render(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        let skip = text.chars().count().saturating_sub(TAIL_CHARS);
        text.chars().skip(skip).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    #[cfg(unix)]
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[tokio::test]
    async fn run_with_empty_argv_fails() {
        let result = run(&[], None, TEST_TIMEOUT).await;

        assert!(!result.ok);
        assert_eq!(result.tail, "no command is configured");
    }

    #[tokio::test]
    async fn run_with_missing_binary_fails_with_start_error() {
        let missing = std::env::temp_dir()
            .join("llm-hub-missing-command")
            .join("apply");
        let missing = missing.to_string_lossy().into_owned();

        let result = run(std::slice::from_ref(&missing), None, TEST_TIMEOUT).await;

        assert!(!result.ok);
        assert!(
            result
                .tail
                .starts_with(&format!("{missing} could not start:")),
            "{}",
            result.tail
        );
    }

    #[test]
    fn tail_buffer_keeps_last_chars_of_multibyte_text() {
        let mut tail = TailBuffer::default();
        for _ in 0..TAIL_CHARS {
            tail.push("é".repeat(10).as_bytes());
        }
        tail.push(b"end");

        let rendered = tail.render();

        assert_eq!(rendered.chars().count(), TAIL_CHARS);
        assert!(rendered.ends_with("éend"), "{rendered}");
        assert!(tail.bytes.len() <= TAIL_BYTES * 2);
    }

    #[test]
    fn tail_buffer_push_line_starts_a_new_line_once() {
        let mut tail = TailBuffer::default();
        tail.push_line("first");
        tail.push(b"\npartial");
        tail.push_line("status");

        assert_eq!(tail.render(), "first\npartial\nstatus");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use crate::services::process_gh_cli::tests::unix::process_alive;

        fn shell(script: &str) -> Vec<String> {
            argv(&["sh", "-c", script])
        }

        #[tokio::test]
        async fn run_success_returns_combined_output() {
            let result = run(&shell("echo out; echo err >&2"), None, TEST_TIMEOUT).await;

            assert!(result.ok, "{}", result.tail);
            assert!(result.tail.contains("out\n"), "{}", result.tail);
            assert!(result.tail.contains("err\n"), "{}", result.tail);
        }

        #[tokio::test]
        async fn run_non_zero_exit_fails_with_status_line() {
            let result = run(&shell("echo broken >&2; exit 3"), None, TEST_TIMEOUT).await;

            assert!(!result.ok);
            assert!(
                result.tail.starts_with("broken\nsh exited with "),
                "{}",
                result.tail
            );
            assert!(result.tail.contains('3'), "{}", result.tail);
        }

        #[tokio::test]
        async fn run_passes_stdin_to_the_command() {
            let result = run(
                &shell("cat"),
                Some(br#"{"id":"n1"}"#.to_vec()),
                TEST_TIMEOUT,
            )
            .await;

            assert!(result.ok, "{}", result.tail);
            assert_eq!(result.tail, r#"{"id":"n1"}"#);
        }

        #[tokio::test]
        async fn run_without_stdin_gives_the_command_empty_input() {
            let result = run(&shell("cat; echo done"), None, TEST_TIMEOUT).await;

            assert!(result.ok, "{}", result.tail);
            assert_eq!(result.tail, "done\n");
        }

        #[tokio::test]
        async fn run_with_ignored_stdin_still_succeeds() {
            let result = run(
                &shell("echo skipped"),
                Some(vec![b'x'; 1_000_000]),
                TEST_TIMEOUT,
            )
            .await;

            assert!(result.ok, "{}", result.tail);
            assert_eq!(result.tail, "skipped\n");
        }

        #[tokio::test]
        async fn run_truncates_output_to_the_last_chars() {
            let result = run(
                &shell("i=0; while [ $i -lt 500 ]; do echo line-$i-xxxxxxxx; i=$((i+1)); done"),
                None,
                TEST_TIMEOUT,
            )
            .await;

            assert!(result.ok, "{}", result.tail);
            assert_eq!(result.tail.chars().count(), TAIL_CHARS);
            assert!(
                result.tail.ends_with("line-499-xxxxxxxx\n"),
                "{}",
                result.tail
            );
            assert!(!result.tail.contains("line-0-"), "{}", result.tail);
        }

        #[tokio::test]
        async fn run_that_never_exits_times_out_and_is_killed() {
            let dir = tempfile::tempdir().unwrap();
            let pid_file = dir.path().join("pid");
            let script = format!(
                "echo started; echo $$ > {}; exec sleep 30",
                pid_file.display()
            );

            let result = run(&shell(&script), None, Duration::from_millis(300)).await;

            assert!(!result.ok);
            assert!(
                result
                    .tail
                    .ends_with("sh did not finish within 300 ms and was killed"),
                "{}",
                result.tail
            );
            // On a loaded machine the timeout can kill the script before it writes its pid.
            if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                assert!(result.tail.starts_with("started\n"), "{}", result.tail);
                assert!(!process_alive(pid.trim()).await, "{pid} still running");
            }
        }
    }
}
