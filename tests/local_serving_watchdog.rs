//! Integration coverage for local stream watchdog behavior.

use std::time::Duration;

use llm_hub::{StreamWatchdog, stall_event};

#[test]
fn the_idle_timer_starts_after_the_first_byte() {
    let mut watchdog = StreamWatchdog::new(Some(Duration::from_secs(30)), Duration::from_secs(5));

    assert_eq!(watchdog.active_timeout(), Some(Duration::from_secs(30)));
    watchdog.received_bytes();
    assert_eq!(watchdog.active_timeout(), Some(Duration::from_secs(5)));
}

#[test]
fn a_stall_event_has_the_client_error_format() {
    let event = String::from_utf8(stall_event("model stalled").to_vec()).unwrap();

    assert!(event.starts_with("data: {\"error\":"));
    assert!(event.ends_with("}}\n\n"));
}
