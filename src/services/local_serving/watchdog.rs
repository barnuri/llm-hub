//! Watchdog timers for local model streams.

use std::time::Duration;

use axum::body::Bytes;

/// A watchdog for one local model stream.
#[derive(Debug)]
pub struct StreamWatchdog {
    first_byte_deadline: Option<Duration>,
    idle_timeout: Duration,
    received_bytes: bool,
}

impl StreamWatchdog {
    /// Creates a watchdog with a first-byte budget and an idle timeout.
    #[must_use]
    pub fn new(first_byte_deadline: Option<Duration>, idle_timeout: Duration) -> Self {
        Self {
            first_byte_deadline,
            idle_timeout,
            received_bytes: false,
        }
    }

    /// Records that the stream produced bytes.
    pub fn received_bytes(&mut self) {
        self.received_bytes = true;
    }

    /// Returns the active timeout: first-byte until bytes arrive, then idle.
    #[must_use]
    pub fn active_timeout(&self) -> Option<Duration> {
        if self.received_bytes {
            Some(self.idle_timeout)
        } else {
            self.first_byte_deadline
        }
    }
}

/// Builds an SSE error event for a watchdog stall.
#[must_use]
pub fn stall_event(message: &str) -> Bytes {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": "stream_error"
        }
    });
    Bytes::from(format!("data: {body}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_timer_starts_only_after_the_first_byte() {
        let mut watchdog =
            StreamWatchdog::new(Some(Duration::from_secs(30)), Duration::from_secs(5));
        assert_eq!(watchdog.active_timeout(), Some(Duration::from_secs(30)));
        watchdog.received_bytes();
        assert_eq!(watchdog.active_timeout(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn stall_event_uses_the_client_error_format() {
        let event = String::from_utf8(stall_event("model stalled").to_vec()).unwrap();
        assert!(event.starts_with("data: {\"error\":"));
        assert!(event.ends_with("}}\n\n"));
    }
}
