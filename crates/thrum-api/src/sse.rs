//! Server-Sent Events (SSE) endpoint for real-time pipeline observability.
//!
//! Subscribes to the broadcast `EventBus` and emits each `PipelineEvent`
//! as a JSON-encoded SSE event. Clients (including the HTMX live dashboard)
//! connect via `GET /api/v1/events/stream`.
//!
//! SSE event format:
//! ```text
//! event: pipeline_event
//! data: {"timestamp":"...","kind":{"AgentStarted":{...}}}
//!
//! ```

use axum::{
    extract::State,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::ApiState;

/// GET /api/v1/events/stream
///
/// Returns an SSE stream of pipeline events. Each event is sent with
/// `event: pipeline_event` and a JSON-encoded `data` payload.
///
/// The stream automatically sends keep-alive comments every 15 seconds
/// to prevent proxy/load-balancer timeouts. If a client falls behind,
/// lagged events are skipped gracefully.
pub async fn event_stream(State(state): State<Arc<ApiState>>) -> impl IntoResponse {
    let rx = state.event_bus.subscribe();
    let stream = BroadcastStream::new(rx);

    let sse_stream = stream.filter_map(|result| match result {
        Ok(event) => {
            let json = serde_json::to_string(&event).ok()?;
            Some(Ok::<_, Infallible>(
                Event::default().event("pipeline_event").data(json),
            ))
        }
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::debug!(skipped = n, "SSE client lagged, skipping events");
            // Send a notification that events were skipped
            Some(Ok(Event::default()
                .event("lagged")
                .data(format!("{{\"skipped\":{n}}}"))))
        }
    });

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use thrum_core::event::{EventKind, LogLevel};
    use tower::ServiceExt;

    fn test_state() -> (Arc<ApiState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let state = Arc::new(ApiState::new(&db_path, dir.path().join("traces"), None).unwrap());
        (state, dir)
    }

    #[tokio::test]
    async fn sse_endpoint_returns_event_stream() {
        let (state, _dir) = test_state();

        // Emit an event before connecting (will be missed, that's expected)
        // Then emit after connecting
        let event_bus = state.event_bus.clone();

        let app = crate::api_router(state);

        // Spawn event emission with a small delay
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            event_bus.emit(EventKind::EngineLog {
                level: LogLevel::Info,
                message: "test SSE event".into(),
            });
        });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/event-stream"), "content-type was: {ct}");
    }
}
