//! Stops the hub when the process that started it dies. A parent that sets
//! `LLM_HUB_EXIT_ON_STDIN_EOF=1` keeps a pipe on the hub's stdin. The pipe closes when the parent
//! exits, even after a crash or SIGKILL, and the hub then shuts down gracefully. Test harnesses use it
//! so a killed test process leaves no hub behind.

use std::io::Read;

/// The variable that turns the watch on. Only `1` and `true` enable it.
pub const EXIT_ON_STDIN_EOF_VAR: &str = "LLM_HUB_EXIT_ON_STDIN_EOF";

/// True when `value` of [`EXIT_ON_STDIN_EOF_VAR`] turns the watch on.
#[must_use]
pub fn enabled(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true"))
}

/// Resolves when `reader` reaches end of file or fails. Bytes read before that are discarded.
/// The read runs on a detached thread, not the runtime's blocking pool, so a read still waiting
/// never holds up the runtime's shutdown.
pub async fn wait_for_eof(mut reader: impl Read + Send + 'static) {
    let (done, closed) = tokio::sync::oneshot::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("parent-watch".to_owned())
        .spawn(move || {
            let mut buffer = [0_u8; 256];
            while matches!(reader.read(&mut buffer), Ok(n) if n > 0) {}
            let _ = done.send(());
        });
    if spawned.is_err() {
        tracing::warn!("could not start the parent watch thread");
        std::future::pending::<()>().await;
    }
    let _ = closed.await;
}

/// Resolves when stdin closes and the watch is on in `vars`. Never resolves when it is off.
pub async fn wait_for_parent_exit(vars: &std::collections::HashMap<String, String>) {
    if enabled(vars.get(EXIT_ON_STDIN_EOF_VAR).map(String::as_str)) {
        wait_for_eof(std::io::stdin()).await;
        tracing::info!("stdin closed: the parent process exited, shutting down");
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use std::io::Write;

    use super::*;

    #[test]
    fn only_one_and_true_enable_the_watch() {
        assert!(enabled(Some("1")));
        assert!(enabled(Some("true")));
        assert!(!enabled(Some("0")));
        assert!(!enabled(Some("")));
        assert!(!enabled(Some("yes")));
        assert!(!enabled(None));
    }

    #[tokio::test]
    async fn eof_resolves_after_the_writer_closes() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let wait = tokio::spawn(wait_for_eof(reader));
        writer.write_all(b"keep alive").unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!wait.is_finished(), "bytes alone must not end the wait");

        drop(writer);

        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn the_watch_never_resolves_when_it_is_off() {
        let vars = HashMap::from([(EXIT_ON_STDIN_EOF_VAR.to_owned(), "0".to_owned())]);

        let waited =
            tokio::time::timeout(Duration::from_millis(20), wait_for_parent_exit(&vars)).await;

        assert!(waited.is_err());
    }
}
