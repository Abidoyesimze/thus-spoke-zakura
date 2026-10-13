use crate::state::AppState;
use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use std::time::Duration;
use tokio::sync::broadcast;
pub(crate) async fn events(
    State(state): State<AppState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut receiver = state.0.events.subscribe();
    let stream = async_stream::stream! { loop { match receiver.recv().await { Ok(data) => yield Ok(Event::default().event("update").data(data)), Err(broadcast::error::RecvError::Lagged(_)) => { yield Ok(Event::default().event("update").data("resync")); }, Err(_) => break } } };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
#[path = "../../tests/unit/api/events/tests.rs"]
mod tests;
