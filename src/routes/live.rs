use std::convert::Infallible;

use axum::extract::State;
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use futures_util::Stream;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;

use crate::consts::{LIVE_KEEP_ALIVE_INTERVAL, LIVE_KEEP_ALIVE_TEXT};
use crate::dependencies::state::AppState;
use crate::schemas::usage_row::UsageRow;

pub async fn stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let events = futures_util::stream::unfold(state.live_feed().subscribe(), next_event);
    Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(LIVE_KEEP_ALIVE_INTERVAL)
            .text(LIVE_KEEP_ALIVE_TEXT),
    )
}

/// Waits for the next row. A lagging receiver skips the rows it missed; a closed channel ends the stream.
async fn next_event(
    mut receiver: Receiver<UsageRow>,
) -> Option<(Result<Event, Infallible>, Receiver<UsageRow>)> {
    loop {
        match receiver.recv().await {
            Ok(row) => {
                if let Ok(event) = Event::default().json_data(&row) {
                    return Some((Ok(event), receiver));
                }
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return None,
        }
    }
}
