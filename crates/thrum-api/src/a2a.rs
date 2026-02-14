//! A2A (Agent-to-Agent) protocol HTTP handlers.
//!
//! Implements:
//! - `GET /.well-known/agent.json` — Agent Card discovery
//! - `POST /a2a` — JSON-RPC 2.0 dispatch (SendMessage, GetTask, ListTasks, CancelTask)
//! - `GET /a2a/subscribe/{task_id}` — SSE stream for a specific task
//! - `POST /a2a/stream` — SSE stream with task creation (SendMessage + subscribe)

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
};
use chrono::Utc;
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use thrum_core::a2a::*;
use thrum_core::event::EventKind;
use thrum_core::task::{RepoName, Task, TaskId, TaskStatus};
use thrum_db::task_store::TaskStore;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::ApiState;

// ─── Agent Card ─────────────────────────────────────────────────────────

/// `GET /.well-known/agent.json`
///
/// Returns the A2A Agent Card describing Thrum's capabilities.
pub async fn agent_card(headers: HeaderMap) -> Json<AgentCard> {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:3000");
    let base_url = format!("http://{host}");
    Json(AgentCard::thrum_default(&base_url))
}

// ─── JSON-RPC Dispatch ──────────────────────────────────────────────────

/// `POST /a2a`
///
/// Single JSON-RPC 2.0 endpoint dispatching on `method`:
/// - `a2a.SendMessage` — create or update a task
/// - `a2a.GetTask` — retrieve a task by ID
/// - `a2a.ListTasks` — list tasks, optionally filtered by context_id
/// - `a2a.CancelTask` — cancel (reject) a non-terminal task
pub async fn jsonrpc_handler(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<serde_json::Value>,
) -> Json<JsonRpcResponse> {
    // Parse as JsonRpcRequest
    let request: JsonRpcRequest = match serde_json::from_value(req) {
        Ok(r) => r,
        Err(e) => {
            return Json(JsonRpcResponse::error(
                serde_json::Value::Null,
                PARSE_ERROR,
                format!("invalid JSON-RPC request: {e}"),
            ));
        }
    };

    if request.jsonrpc != "2.0" {
        return Json(JsonRpcResponse::error(
            request.id,
            INVALID_REQUEST,
            "jsonrpc must be \"2.0\"",
        ));
    }

    let result = match request.method.as_str() {
        "a2a.SendMessage" => handle_send_message(&state, &request).await,
        "a2a.GetTask" => handle_get_task(&state, &request),
        "a2a.ListTasks" => handle_list_tasks(&state, &request),
        "a2a.CancelTask" => handle_cancel_task(&state, &request),
        _ => Err((
            METHOD_NOT_FOUND,
            format!("unknown method: {}", request.method),
        )),
    };

    Json(match result {
        Ok(value) => JsonRpcResponse::success(request.id, value),
        Err((code, msg)) => JsonRpcResponse::error(request.id, code, msg),
    })
}

type RpcResult = Result<serde_json::Value, (i64, String)>;

async fn handle_send_message(state: &ApiState, req: &JsonRpcRequest) -> RpcResult {
    let params: SendMessageParams = serde_json::from_value(req.params.clone())
        .map_err(|e| (INVALID_PARAMS, format!("invalid params: {e}")))?;

    let store = TaskStore::new(state.db());

    // If task_id is provided, return the existing task
    if let Some(ref a2a_id) = params.task_id {
        let task_id = parse_thrum_task_id(a2a_id)
            .ok_or_else(|| (INVALID_PARAMS, format!("invalid task_id: {a2a_id}")))?;
        let task = store
            .get(&task_id)
            .map_err(|e| (INTERNAL_ERROR, format!("db error: {e}")))?
            .ok_or_else(|| (TASK_NOT_FOUND, format!("task {a2a_id} not found")))?;
        return Ok(serde_json::to_value(A2aTask::from_thrum_task(&task)).unwrap());
    }

    // Extract text from message parts for title/description
    let text_parts: Vec<&str> = params
        .message
        .parts
        .iter()
        .filter_map(|p| match p {
            A2aPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    let full_text = text_parts.join("\n");
    let (title, description) = match full_text.split_once('\n') {
        Some((t, d)) => (t.trim().to_string(), d.trim().to_string()),
        None => (full_text.trim().to_string(), String::new()),
    };

    if title.is_empty() {
        return Err((INVALID_PARAMS, "message must contain text".into()));
    }

    // Extract repo from metadata, default to "default"
    let repo = params
        .metadata
        .get("repo")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let mut task = Task::new(RepoName::new(repo), title, description);
    task.context_id = params.context_id;

    let task = store
        .insert(task)
        .map_err(|e| (INTERNAL_ERROR, format!("failed to create task: {e}")))?;

    // Emit event
    state.event_bus.emit(EventKind::TaskStateChange {
        task_id: task.id.clone(),
        repo: task.repo.clone(),
        from: "none".into(),
        to: "pending".into(),
    });

    Ok(serde_json::to_value(A2aTask::from_thrum_task(&task)).unwrap())
}

fn handle_get_task(state: &ApiState, req: &JsonRpcRequest) -> RpcResult {
    let params: GetTaskParams = serde_json::from_value(req.params.clone())
        .map_err(|e| (INVALID_PARAMS, format!("invalid params: {e}")))?;

    let task_id = parse_thrum_task_id(&params.task_id).ok_or_else(|| {
        (
            INVALID_PARAMS,
            format!("invalid task_id: {}", params.task_id),
        )
    })?;

    let store = TaskStore::new(state.db());
    let task = store
        .get(&task_id)
        .map_err(|e| (INTERNAL_ERROR, format!("db error: {e}")))?
        .ok_or_else(|| (TASK_NOT_FOUND, format!("task {} not found", params.task_id)))?;

    Ok(serde_json::to_value(A2aTask::from_thrum_task(&task)).unwrap())
}

fn handle_list_tasks(state: &ApiState, req: &JsonRpcRequest) -> RpcResult {
    let params: ListTasksParams =
        serde_json::from_value(req.params.clone()).unwrap_or(ListTasksParams { context_id: None });

    let store = TaskStore::new(state.db());
    let tasks = store
        .list(None, None)
        .map_err(|e| (INTERNAL_ERROR, format!("db error: {e}")))?;

    let a2a_tasks: Vec<A2aTask> = tasks
        .iter()
        .filter(|t| {
            if let Some(ref ctx) = params.context_id {
                a2a_context_id(t) == *ctx
            } else {
                true
            }
        })
        .map(A2aTask::from_thrum_task)
        .collect();

    Ok(serde_json::to_value(a2a_tasks).unwrap())
}

fn handle_cancel_task(state: &ApiState, req: &JsonRpcRequest) -> RpcResult {
    let params: CancelTaskParams = serde_json::from_value(req.params.clone())
        .map_err(|e| (INVALID_PARAMS, format!("invalid params: {e}")))?;

    let task_id = parse_thrum_task_id(&params.task_id).ok_or_else(|| {
        (
            INVALID_PARAMS,
            format!("invalid task_id: {}", params.task_id),
        )
    })?;

    let store = TaskStore::new(state.db());
    let mut task = store
        .get(&task_id)
        .map_err(|e| (INTERNAL_ERROR, format!("db error: {e}")))?
        .ok_or_else(|| (TASK_NOT_FOUND, format!("task {} not found", params.task_id)))?;

    if task.status.is_terminal() {
        return Err((
            TASK_NOT_CANCELABLE,
            "task is already in a terminal state".into(),
        ));
    }

    let from = task.status.label().to_string();
    task.status = TaskStatus::Rejected {
        feedback: "canceled via A2A".into(),
    };
    task.updated_at = Utc::now();
    store
        .update(&task)
        .map_err(|e| (INTERNAL_ERROR, format!("failed to update task: {e}")))?;

    state.event_bus.emit(EventKind::TaskStateChange {
        task_id: task.id.clone(),
        repo: task.repo.clone(),
        from,
        to: "rejected".into(),
    });

    Ok(serde_json::to_value(A2aTask::from_thrum_task(&task)).unwrap())
}

// ─── SSE Subscribe ──────────────────────────────────────────────────────

/// `GET /a2a/subscribe/{task_id}`
///
/// SSE stream of A2A events for a specific task. Filters the EventBus
/// for events matching the given task ID.
pub async fn subscribe_handler(
    State(state): State<Arc<ApiState>>,
    Path(a2a_id): Path<String>,
) -> impl IntoResponse {
    let target_id = parse_thrum_task_id(&a2a_id);
    let rx = state.event_bus.subscribe();
    let stream = BroadcastStream::new(rx);

    let sse_stream = stream.filter_map(move |result| {
        let target_id = target_id.clone();
        match result {
            Ok(event) => {
                let a2a_event = pipeline_event_to_a2a(&event.kind, target_id.as_ref()?)?;
                let json = serde_json::to_string(&a2a_event).ok()?;
                Some(Ok::<_, Infallible>(
                    Event::default().event("a2a_event").data(json),
                ))
            }
            Err(_) => None,
        }
    });

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

// ─── SSE Streaming (SendMessage + subscribe) ────────────────────────────

/// `POST /a2a/stream`
///
/// Creates a task via SendMessage semantics, then returns an SSE stream
/// of A2A events for that task. The first event is always a `task` event
/// with the full task state.
pub async fn streaming_handler(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Subscribe before creating the task to avoid missing the initial event
    let rx = state.event_bus.subscribe();

    // Parse and create task using the same logic as SendMessage
    let request: JsonRpcRequest = match serde_json::from_value(req) {
        Ok(r) => r,
        Err(_) => {
            return Json(JsonRpcResponse::error(
                serde_json::Value::Null,
                PARSE_ERROR,
                "invalid JSON-RPC request",
            ))
            .into_response();
        }
    };

    let task_result = handle_send_message(&state, &request).await;

    let (a2a_task, task_id) = match task_result {
        Ok(value) => {
            let a2a_task: A2aTask = serde_json::from_value(value).unwrap();
            let task_id = parse_thrum_task_id(&a2a_task.id);
            (a2a_task, task_id)
        }
        Err((code, msg)) => {
            return Json(JsonRpcResponse::error(request.id, code, msg)).into_response();
        }
    };

    // Initial task event
    let initial = A2aStreamEvent::Task { task: a2a_task };
    let initial_json = serde_json::to_string(&initial).unwrap();
    let initial_event = Event::default().event("a2a_event").data(initial_json);

    // Follow-up events filtered from EventBus
    let follow_stream = BroadcastStream::new(rx).filter_map(move |result| {
        let task_id = task_id.clone();
        match result {
            Ok(event) => {
                let a2a_event = pipeline_event_to_a2a(&event.kind, task_id.as_ref()?)?;
                let json = serde_json::to_string(&a2a_event).ok()?;
                Some(Ok::<_, Infallible>(
                    Event::default().event("a2a_event").data(json),
                ))
            }
            Err(_) => None,
        }
    });

    let combined = tokio_stream::once(Ok(initial_event)).chain(follow_stream);
    Sse::new(combined)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ─── Event Conversion ───────────────────────────────────────────────────

/// Convert a Thrum `EventKind` to an A2A stream event.
///
/// Returns `None` if the event doesn't match the target task or isn't
/// relevant for A2A streaming.
fn pipeline_event_to_a2a(kind: &EventKind, target: &TaskId) -> Option<A2aStreamEvent> {
    match kind {
        EventKind::TaskStateChange {
            task_id, from, to, ..
        } if task_id == target => Some(A2aStreamEvent::StatusUpdate {
            task_id: a2a_task_id(task_id),
            status: A2aTaskStatus {
                state: label_to_a2a_state(to),
                timestamp: Utc::now(),
                message: Some(format!("{from} -> {to}")),
            },
        }),

        EventKind::AgentOutput { task_id, line, .. } if task_id == target => {
            Some(A2aStreamEvent::Message {
                message: A2aMessage {
                    message_id: next_message_id(),
                    role: A2aRole::Agent,
                    parts: vec![A2aPart::Text { text: line.clone() }],
                    metadata: HashMap::new(),
                },
            })
        }

        EventKind::AgentFinished {
            task_id,
            success,
            elapsed_secs,
            ..
        } if task_id == target => {
            let state = if *success {
                A2aTaskState::Working
            } else {
                A2aTaskState::Failed
            };
            Some(A2aStreamEvent::StatusUpdate {
                task_id: a2a_task_id(task_id),
                status: A2aTaskStatus {
                    state,
                    timestamp: Utc::now(),
                    message: Some(format!(
                        "Agent finished ({}, {elapsed_secs:.1}s)",
                        if *success { "success" } else { "failed" }
                    )),
                },
            })
        }

        EventKind::DiffUpdate {
            task_id,
            files_changed,
            insertions,
            deletions,
            ..
        } if task_id == target => Some(A2aStreamEvent::ArtifactUpdate {
            task_id: a2a_task_id(task_id),
            artifact: A2aArtifact {
                artifact_id: next_artifact_id(),
                name: "diff".into(),
                parts: vec![A2aPart::Data {
                    data: serde_json::json!({
                        "files_changed": files_changed,
                        "insertions": insertions,
                        "deletions": deletions,
                    }),
                }],
                metadata: HashMap::new(),
            },
        }),

        EventKind::GateFinished {
            task_id,
            level,
            passed,
            duration_secs,
        } if task_id == target => Some(A2aStreamEvent::ArtifactUpdate {
            task_id: a2a_task_id(task_id),
            artifact: A2aArtifact {
                artifact_id: next_artifact_id(),
                name: format!("gate-{}", level),
                parts: vec![A2aPart::Data {
                    data: serde_json::json!({
                        "level": format!("{level}"),
                        "passed": passed,
                        "duration_secs": duration_secs,
                    }),
                }],
                metadata: HashMap::new(),
            },
        }),

        _ => None,
    }
}

/// Map a status label string back to an A2A state.
fn label_to_a2a_state(label: &str) -> A2aTaskState {
    match label {
        "pending" | "claimed" => A2aTaskState::Submitted,
        "implementing" | "reviewing" | "approved" | "integrating" => A2aTaskState::Working,
        "gate1-failed" | "gate2-failed" | "gate3-failed" => A2aTaskState::Failed,
        "awaiting-approval" => A2aTaskState::InputRequired,
        "merged" => A2aTaskState::Completed,
        "rejected" => A2aTaskState::Rejected,
        _ => A2aTaskState::Working,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state() -> (Arc<ApiState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let state = Arc::new(ApiState::new(&db_path, dir.path().join("traces"), None).unwrap());
        (state, dir)
    }

    #[tokio::test]
    async fn agent_card_returns_200() {
        let (state, _dir) = test_state();
        let app = crate::api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/agent.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let card: AgentCard = serde_json::from_slice(&body).unwrap();
        assert_eq!(card.name, "Thrum");
        assert_eq!(card.skills.len(), 3);
        assert!(card.capabilities.streaming);
    }

    #[tokio::test]
    async fn send_message_creates_task() {
        let (state, _dir) = test_state();
        let app = crate::api_router(state);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.SendMessage",
            "params": {
                "message": {
                    "message_id": "m1",
                    "role": "user",
                    "parts": [{"type": "text", "text": "Implement feature X\nDetailed description here"}]
                },
                "metadata": {"repo": "loom"}
            }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(resp.error.is_none());
        let task: A2aTask = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(task.status.state, A2aTaskState::Submitted);
        assert!(task.id.starts_with("thrum-"));
        assert_eq!(task.metadata["repo"], "loom");
    }

    #[tokio::test]
    async fn get_task_returns_task() {
        let (state, _dir) = test_state();

        // Insert a task
        let task_id = {
            let store = TaskStore::new(state.db());
            let task = Task::new(RepoName::new("loom"), "Test".into(), "desc".into());
            store.insert(task).unwrap().id
        };

        let app = crate::api_router(state);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.GetTask",
            "params": {"task_id": a2a_task_id(&task_id)}
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(resp.error.is_none());
        let task: A2aTask = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(task.id, a2a_task_id(&task_id));
    }

    #[tokio::test]
    async fn list_tasks_returns_all() {
        let (state, _dir) = test_state();

        // Insert two tasks
        {
            let store = TaskStore::new(state.db());
            store
                .insert(Task::new(RepoName::new("loom"), "T1".into(), "d1".into()))
                .unwrap();
            store
                .insert(Task::new(RepoName::new("synth"), "T2".into(), "d2".into()))
                .unwrap();
        }

        let app = crate::api_router(state);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.ListTasks",
            "params": {}
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        let tasks: Vec<A2aTask> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[tokio::test]
    async fn cancel_task_rejects() {
        let (state, _dir) = test_state();

        let task_id = {
            let store = TaskStore::new(state.db());
            let task = Task::new(RepoName::new("loom"), "Test".into(), "desc".into());
            store.insert(task).unwrap().id
        };

        let app = crate::api_router(state);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.CancelTask",
            "params": {"task_id": a2a_task_id(&task_id)}
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        let task: A2aTask = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(task.status.state, A2aTaskState::Rejected);
    }

    #[tokio::test]
    async fn subscribe_returns_event_stream() {
        let (state, _dir) = test_state();

        // Insert a task
        {
            let store = TaskStore::new(state.db());
            let task = Task::new(RepoName::new("loom"), "Test".into(), "desc".into());
            store.insert(task).unwrap();
        }

        let app = crate::api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/a2a/subscribe/thrum-1")
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
        assert!(
            ct.contains("text/event-stream"),
            "expected text/event-stream, got: {ct}"
        );
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let (state, _dir) = test_state();
        let app = crate::api_router(state);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.DoesNotExist",
            "params": {}
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn get_nonexistent_task_returns_error() {
        let (state, _dir) = test_state();
        let app = crate::api_router(state);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.GetTask",
            "params": {"task_id": "thrum-9999"}
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, TASK_NOT_FOUND);
    }

    #[tokio::test]
    async fn cancel_terminal_task_returns_error() {
        let (state, _dir) = test_state();

        // Insert and immediately merge a task
        let task_id = {
            let store = TaskStore::new(state.db());
            let task = Task::new(RepoName::new("loom"), "Test".into(), "desc".into());
            let task = store.insert(task).unwrap();
            let mut t = task.clone();
            t.status = TaskStatus::Merged {
                commit_sha: "abc123".into(),
            };
            t.updated_at = Utc::now();
            store.update(&t).unwrap();
            t.id
        };

        let app = crate::api_router(state);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.CancelTask",
            "params": {"task_id": a2a_task_id(&task_id)}
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/a2a")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, TASK_NOT_CANCELABLE);
    }

    #[test]
    fn event_conversion_task_state_change() {
        let event = EventKind::TaskStateChange {
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
            from: "pending".into(),
            to: "implementing".into(),
        };
        let result = pipeline_event_to_a2a(&event, &TaskId(1));
        assert!(result.is_some());
        assert!(matches!(
            result.unwrap(),
            A2aStreamEvent::StatusUpdate { .. }
        ));
    }

    #[test]
    fn event_conversion_wrong_task_returns_none() {
        let event = EventKind::TaskStateChange {
            task_id: TaskId(1),
            repo: RepoName::new("loom"),
            from: "pending".into(),
            to: "implementing".into(),
        };
        let result = pipeline_event_to_a2a(&event, &TaskId(99));
        assert!(result.is_none());
    }

    #[test]
    fn event_conversion_agent_output() {
        let event = EventKind::AgentOutput {
            agent_id: thrum_core::agent::AgentId("a1".into()),
            task_id: TaskId(1),
            stream: thrum_core::event::OutputStream::Stdout,
            line: "compiling...".into(),
        };
        let result = pipeline_event_to_a2a(&event, &TaskId(1));
        assert!(matches!(result, Some(A2aStreamEvent::Message { .. })));
    }

    #[test]
    fn event_conversion_diff_update() {
        let event = EventKind::DiffUpdate {
            agent_id: thrum_core::agent::AgentId("a1".into()),
            task_id: TaskId(1),
            files_changed: 3,
            insertions: 42,
            deletions: 7,
        };
        let result = pipeline_event_to_a2a(&event, &TaskId(1));
        assert!(matches!(
            result,
            Some(A2aStreamEvent::ArtifactUpdate { .. })
        ));
    }

    #[test]
    fn label_to_state_coverage() {
        assert_eq!(label_to_a2a_state("pending"), A2aTaskState::Submitted);
        assert_eq!(label_to_a2a_state("implementing"), A2aTaskState::Working);
        assert_eq!(label_to_a2a_state("gate1-failed"), A2aTaskState::Failed);
        assert_eq!(
            label_to_a2a_state("awaiting-approval"),
            A2aTaskState::InputRequired
        );
        assert_eq!(label_to_a2a_state("merged"), A2aTaskState::Completed);
        assert_eq!(label_to_a2a_state("rejected"), A2aTaskState::Rejected);
    }
}
