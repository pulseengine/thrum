//! HTTP API for thrum, enabling external integrations
//! (OpenClawed, Telegram bots, CI/CD webhooks).
//!
//! Built with axum for async HTTP serving.
//! Includes an embedded HTMX-powered dashboard at `/dashboard`.

mod a2a;
mod dashboard;
mod sse;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use thrum_core::repo::ReposConfig;
use thrum_core::task::{RepoName, Task, TaskId, TaskStatus};
use thrum_core::telemetry::{TraceFilter, TraceReader};
use thrum_db::task_store::TaskStore;
use thrum_runner::event_bus::EventBus;
use thrum_runner::git::GitRepo;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

/// Shared application state for API handlers.
///
/// Holds an `Arc<Database>` opened once at server start, shared across all
/// request handlers. This avoids the overhead of reopening the database on
/// every request and prevents per-request file lock churn.
pub struct ApiState {
    pub db: Arc<redb::Database>,
    pub trace_dir: PathBuf,
    /// Path to repos.toml — needed for diff endpoint to locate repo working dirs.
    pub config_path: Option<PathBuf>,
    /// Event bus for real-time SSE streaming of pipeline events.
    pub event_bus: EventBus,
}

impl ApiState {
    /// Create ApiState with a shared database connection.
    pub fn new(
        db_path: &std::path::Path,
        trace_dir: PathBuf,
        config_path: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let db = Arc::new(thrum_db::open_db(db_path)?);
        Ok(Self {
            db,
            trace_dir,
            config_path,
            event_bus: EventBus::new(),
        })
    }

    /// Create ApiState with a pre-existing EventBus (for sharing with the engine).
    pub fn with_event_bus(
        db_path: &std::path::Path,
        trace_dir: PathBuf,
        config_path: Option<PathBuf>,
        event_bus: EventBus,
    ) -> anyhow::Result<Self> {
        let db = Arc::new(thrum_db::open_db(db_path)?);
        Ok(Self {
            db,
            trace_dir,
            config_path,
            event_bus,
        })
    }

    /// Create ApiState with a pre-opened shared database and EventBus.
    ///
    /// Used when the engine already holds the DB lock (redb is exclusive)
    /// and we need to share the same handle with the API server.
    pub fn with_shared_db(
        db: Arc<redb::Database>,
        trace_dir: PathBuf,
        config_path: Option<PathBuf>,
        event_bus: EventBus,
    ) -> Self {
        Self {
            db,
            trace_dir,
            config_path,
            event_bus,
        }
    }

    /// Get a reference to the shared database.
    fn db(&self) -> &redb::Database {
        &self.db
    }

    /// Load repos configuration. Returns an error if config_path is not set.
    fn repos_config(&self) -> Result<ReposConfig, AppError> {
        let path = self
            .config_path
            .as_ref()
            .ok_or_else(|| AppError::internal("repos config path not configured"))?;
        ReposConfig::load(path).map_err(AppError::Internal)
    }
}

/// Build the axum router with all API routes and the embedded dashboard.
pub fn api_router(state: Arc<ApiState>) -> Router {
    Router::new()
        // JSON API
        .route("/api/v1/health", get(health_check))
        .route("/api/v1/status", get(status))
        .route("/api/v1/tasks", get(list_tasks).post(create_task))
        .route("/api/v1/tasks/{id}", get(get_task))
        .route("/api/v1/tasks/{id}/diff", get(get_task_diff))
        .route("/api/v1/tasks/{id}/approve", post(approve_task))
        .route("/api/v1/tasks/{id}/reject", post(reject_task))
        .route("/api/v1/traces", get(list_traces))
        // SSE event stream
        .route("/api/v1/events/stream", get(sse::event_stream))
        // A2A protocol endpoints
        .route("/.well-known/agent.json", get(a2a::agent_card))
        .route("/a2a", post(a2a::jsonrpc_handler))
        .route("/a2a/stream", post(a2a::streaming_handler))
        .route("/a2a/subscribe/{task_id}", get(a2a::subscribe_handler))
        // Embedded web dashboard
        .merge(dashboard::dashboard_router())
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the API server.
pub async fn serve(state: Arc<ApiState>, bind_addr: &str) -> anyhow::Result<()> {
    let app = api_router(state);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "starting API server");
    tracing::info!("dashboard available at http://{bind_addr}/dashboard");
    tracing::info!("live dashboard at http://{bind_addr}/dashboard/live");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Start the API server with a graceful shutdown signal.
///
/// When the token is cancelled, the server stops accepting new connections
/// and finishes in-flight requests. Used by `thrum run --serve` so the API server
/// shuts down together with the engine, releasing the shared DB lock.
pub async fn serve_with_shutdown(
    state: Arc<ApiState>,
    bind_addr: &str,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let app = api_router(state);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "starting API server (with graceful shutdown)");
    tracing::info!("dashboard available at http://{bind_addr}/dashboard");
    tracing::info!("live dashboard at http://{bind_addr}/dashboard/live");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
    tracing::info!("API server shut down gracefully");
    Ok(())
}

// ─── Error type ──────────────────────────────────────────────────────────

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    error: anyhow::Error,
}

impl AppError {
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: anyhow::anyhow!(msg.into()),
        }
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: anyhow::anyhow!(msg.into()),
        }
    }

    #[allow(non_snake_case)]
    fn Internal(e: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: e,
        }
    }
}

impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({ "error": self.error.to_string() });
        (self.status, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: e.into(),
        }
    }
}

// ─── Health & Status ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Serialize)]
struct StatusResponse {
    healthy: bool,
    task_counts: std::collections::HashMap<String, usize>,
    db_path: String,
    trace_dir: String,
    /// Remaining budget in USD, if a budget tracker has been persisted.
    budget_remaining: Option<f64>,
    /// Total budget ceiling in USD, if a budget tracker has been persisted.
    budget_ceiling: Option<f64>,
    /// Total spent so far in USD, if a budget tracker has been persisted.
    budget_spent: Option<f64>,
}

async fn status(State(state): State<Arc<ApiState>>) -> Result<Json<StatusResponse>, AppError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let counts = store.status_counts()?;

    // Load budget info from the budget store
    let budget_store = thrum_db::budget_store::BudgetStore::new(db);
    let (budget_remaining, budget_ceiling, budget_spent) = match budget_store.load() {
        Ok(Some(tracker)) => (
            Some(tracker.remaining()),
            Some(tracker.ceiling_usd),
            Some(tracker.total_spent()),
        ),
        _ => (None, None, None),
    };

    Ok(Json(StatusResponse {
        healthy: true,
        task_counts: counts,
        db_path: "(shared)".to_string(),
        trace_dir: state.trace_dir.display().to_string(),
        budget_remaining,
        budget_ceiling,
        budget_spent,
    }))
}

// ─── Tasks ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TaskListQuery {
    status: Option<String>,
    repo: Option<String>,
}

#[derive(Serialize)]
struct TaskResponse {
    id: i64,
    repo: String,
    title: String,
    description: String,
    status: String,
    retry_count: u32,
    requirement_id: Option<String>,
    acceptance_criteria: Vec<String>,
    created_at: String,
    updated_at: String,
}

impl From<Task> for TaskResponse {
    fn from(t: Task) -> Self {
        Self {
            id: t.id.0,
            repo: t.repo.to_string(),
            title: t.title,
            description: t.description,
            status: t.status.label().to_string(),
            retry_count: t.retry_count,
            requirement_id: t.requirement_id,
            acceptance_criteria: t.acceptance_criteria,
            created_at: t.created_at.to_rfc3339(),
            updated_at: t.updated_at.to_rfc3339(),
        }
    }
}

async fn list_tasks(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<Vec<TaskResponse>>, AppError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let repo_filter = query
        .repo
        .as_deref()
        .map(|r| r.parse::<RepoName>())
        .transpose()
        .map_err(|e| AppError::internal(format!("invalid repo: {e}")))?;

    let tasks = store.list(query.status.as_deref(), repo_filter.as_ref())?;
    let response: Vec<TaskResponse> = tasks.into_iter().map(TaskResponse::from).collect();
    Ok(Json(response))
}

async fn get_task(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Json<TaskResponse>, AppError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let task = store
        .get(&TaskId(id))?
        .ok_or_else(|| AppError::internal(format!("task {id} not found")))?;

    Ok(Json(TaskResponse::from(task)))
}

#[derive(Deserialize)]
struct CreateTaskRequest {
    repo: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    requirement_id: Option<String>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
}

async fn create_task(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<TaskResponse>), AppError> {
    let repo_name = RepoName::new(&req.repo);

    let db = state.db();
    let store = TaskStore::new(db);

    let mut task = Task::new(repo_name, req.title, req.description);
    task.requirement_id = req.requirement_id;
    task.acceptance_criteria = req.acceptance_criteria;

    let task = store.insert(task)?;
    Ok((StatusCode::CREATED, Json(TaskResponse::from(task))))
}

async fn approve_task(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Json<TaskResponse>, AppError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let mut task = store
        .get(&TaskId(id))?
        .ok_or_else(|| AppError::internal(format!("task {id} not found")))?;

    if !task.status.needs_human() {
        return Err(AppError::internal(format!(
            "task {} is '{}', not awaiting approval",
            id,
            task.status.label()
        )));
    }

    task.status = TaskStatus::Approved;
    task.updated_at = Utc::now();
    store.update(&task)?;

    Ok(Json(TaskResponse::from(task)))
}

#[derive(Deserialize)]
struct RejectRequest {
    feedback: String,
}

async fn reject_task(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    Json(req): Json<RejectRequest>,
) -> Result<Json<TaskResponse>, AppError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let mut task = store
        .get(&TaskId(id))?
        .ok_or_else(|| AppError::internal(format!("task {id} not found")))?;

    task.status = TaskStatus::Rejected {
        feedback: req.feedback,
    };
    task.updated_at = Utc::now();
    store.update(&task)?;

    Ok(Json(TaskResponse::from(task)))
}

// ─── Diff ────────────────────────────────────────────────────────────────

/// GET /api/v1/tasks/{id}/diff
///
/// Returns the full unified diff for a task's branch vs. the default branch.
/// Only available for tasks in `Reviewing` or `AwaitingApproval` state.
/// Returns plain text (not JSON) so it can be displayed directly in chat.
async fn get_task_diff(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let task = store
        .get(&TaskId(id))?
        .ok_or_else(|| AppError::not_found(format!("task {id} not found")))?;

    if !task.status.is_reviewable() {
        return Err(AppError::not_found(format!(
            "task {} is '{}', diff only available for reviewing or awaiting-approval tasks",
            id,
            task.status.label()
        )));
    }

    let repos_config = state.repos_config()?;
    let repo_config = repos_config
        .get(&task.repo)
        .ok_or_else(|| AppError::not_found(format!("repo '{}' not found in config", task.repo)))?;

    let git_repo = GitRepo::open(&repo_config.path)
        .map_err(|e| AppError::internal(format!("failed to open git repo: {e}")))?;

    let branch = task.branch_name();
    let diff = git_repo
        .diff_patch_for_branch(&branch)
        .map_err(|e| AppError::internal(format!("failed to generate diff: {e}")))?;

    Ok((
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        diff,
    ))
}

// ─── Traces ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TracesQuery {
    #[serde(default = "default_trace_limit")]
    limit: usize,
    level: Option<String>,
    target: Option<String>,
}

fn default_trace_limit() -> usize {
    50
}

async fn list_traces(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<TracesQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let reader = TraceReader::new(&state.trace_dir);
    let filter = TraceFilter {
        limit: Some(query.limit),
        level: query.level,
        target_prefix: query.target,
        field_filter: None,
    };

    let events = reader.read_events(&filter)?;
    let json_events: Vec<serde_json::Value> = events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or_default())
        .collect();

    Ok(Json(serde_json::json!({
        "count": json_events.len(),
        "events": json_events,
    })))
}

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
        (state, dir) // Keep dir alive for the test duration
    }

    #[tokio::test]
    async fn health_endpoint() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn task_crud_lifecycle() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        // Create a task
        let create_body = serde_json::json!({
            "repo": "loom",
            "title": "Test task via API",
            "description": "A test"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&create_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn diff_endpoint_returns_404_for_missing_task() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/tasks/999/diff")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn diff_endpoint_returns_404_for_pending_task() {
        let (state, _dir) = test_state();

        // Create a task (starts as Pending)
        {
            let store = TaskStore::new(state.db());
            let task = Task::new(RepoName::new("test"), "Test".into(), "desc".into());
            store.insert(task).unwrap();
        }

        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/tasks/1/diff")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn dashboard_serves_html() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Thrum Dashboard"));
        assert!(html.contains("htmx.org"));
    }

    #[tokio::test]
    async fn dashboard_css_served() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/assets/style.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response.headers().get("content-type").unwrap();
        assert_eq!(ct, "text/css; charset=utf-8");
    }

    #[tokio::test]
    async fn dashboard_status_partial() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/partials/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("status-grid"));
        assert!(html.contains("Pending"));
    }

    #[tokio::test]
    async fn dashboard_tasks_partial_empty() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/partials/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("No tasks in queue"));
    }

    #[tokio::test]
    async fn dashboard_tasks_partial_with_tasks() {
        let (state, _dir) = test_state();

        // Insert a task directly via the shared DB
        {
            let store = TaskStore::new(state.db());
            let task = Task::new(RepoName::new("loom"), "Test task".into(), "desc".into());
            store.insert(task).unwrap();
        }

        let app = api_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/partials/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("task-table"));
        assert!(html.contains("loom"));
        assert!(html.contains("Test task"));
        assert!(html.contains("pending"));
    }

    #[tokio::test]
    async fn dashboard_activity_partial_empty() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/partials/activity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("No activity yet"));
    }

    #[tokio::test]
    async fn sse_endpoint_returns_200() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/events/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
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
    async fn live_dashboard_serves_html() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("Thrum Live Dashboard"));
        assert!(html.contains("EventSource"));
        assert!(html.contains("/api/v1/events/stream"));
    }

    #[tokio::test]
    async fn live_css_served() {
        let (state, _dir) = test_state();
        let app = api_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/assets/live.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response.headers().get("content-type").unwrap();
        assert_eq!(ct, "text/css; charset=utf-8");
    }

    #[tokio::test]
    async fn api_state_with_event_bus() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let event_bus = EventBus::new();
        let state =
            ApiState::with_event_bus(&db_path, dir.path().join("traces"), None, event_bus.clone())
                .unwrap();
        assert_eq!(state.event_bus.subscriber_count(), 0);
        let _rx = state.event_bus.subscribe();
        assert_eq!(event_bus.subscriber_count(), 1);
    }

    // ─── --serve flag verification tests ─────────────────────────────────

    /// Helper: spin up the API server on an OS-assigned port and return the base URL.
    /// The returned `tempfile::TempDir` must be kept alive for the duration of the test.
    async fn start_serve(state: Arc<ApiState>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let handle = tokio::spawn(async move {
            let app = api_router(state);
            axum::serve(listener, app).await.unwrap();
        });

        // Give the server a moment to start accepting connections
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        (base_url, handle)
    }

    #[tokio::test]
    async fn serve_binds_and_responds() {
        let (state, _dir) = test_state();
        let (base_url, _handle) = start_serve(state).await;

        // Hit the health endpoint over real TCP
        let resp = reqwest::get(format!("{base_url}/api/v1/health"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn serve_with_shared_db_exposes_a2a_agent_card() {
        // This exercises the exact ApiState::with_shared_db path that --serve uses
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let shared_db = Arc::new(thrum_db::open_db(&db_path).unwrap());
        let event_bus = EventBus::new();
        let state = Arc::new(ApiState::with_shared_db(
            shared_db,
            dir.path().join("traces"),
            None,
            event_bus,
        ));

        let (base_url, _handle) = start_serve(state).await;

        let resp = reqwest::get(format!("{base_url}/.well-known/agent.json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let card: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(card["name"], "Thrum");
        assert_eq!(card["capabilities"]["streaming"], true);
        // Agent card should advertise 3 skills: implement, review, status
        assert_eq!(card["skills"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn serve_a2a_roundtrip_via_tcp() {
        // Full A2A roundtrip through real HTTP: SendMessage → GetTask
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let shared_db = Arc::new(thrum_db::open_db(&db_path).unwrap());
        let event_bus = EventBus::new();
        let state = Arc::new(ApiState::with_shared_db(
            shared_db,
            dir.path().join("traces"),
            None,
            event_bus,
        ));

        let (base_url, _handle) = start_serve(state).await;

        let client = reqwest::Client::new();

        // 1. Create a task via A2A SendMessage
        let send_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.SendMessage",
            "params": {
                "message": {
                    "message_id": "test-m1",
                    "role": "user",
                    "parts": [{"type": "text", "text": "Verify --serve flag\nEnd-to-end test"}]
                },
                "metadata": {"repo": "test-repo"}
            }
        });

        let resp = client
            .post(format!("{base_url}/a2a"))
            .json(&send_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let rpc_resp: serde_json::Value = resp.json().await.unwrap();
        assert!(rpc_resp["error"].is_null(), "expected no error: {rpc_resp}");
        let task_id = rpc_resp["result"]["id"].as_str().unwrap().to_string();
        assert!(task_id.starts_with("thrum-"));
        assert_eq!(rpc_resp["result"]["status"]["state"], "submitted");
        assert_eq!(rpc_resp["result"]["metadata"]["repo"], "test-repo");

        // 2. Retrieve the same task via A2A GetTask
        let get_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "a2a.GetTask",
            "params": {"task_id": task_id}
        });

        let resp = client
            .post(format!("{base_url}/a2a"))
            .json(&get_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let rpc_resp: serde_json::Value = resp.json().await.unwrap();
        assert!(rpc_resp["error"].is_null());
        assert_eq!(rpc_resp["result"]["id"], task_id);

        // 3. Verify the task also shows up in ListTasks
        let list_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "a2a.ListTasks",
            "params": {}
        });

        let resp = client
            .post(format!("{base_url}/a2a"))
            .json(&list_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let rpc_resp: serde_json::Value = resp.json().await.unwrap();
        let tasks = rpc_resp["result"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], task_id);
    }

    #[tokio::test]
    async fn serve_shared_event_bus_delivers_events() {
        // Verify that events emitted on a shared EventBus reach the API's SSE endpoint.
        // This is the core mechanism that makes --serve useful: the engine emits events
        // on the bus, and the API server streams them to subscribers.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let shared_db = Arc::new(thrum_db::open_db(&db_path).unwrap());
        let event_bus = EventBus::new();
        let state = Arc::new(ApiState::with_shared_db(
            shared_db,
            dir.path().join("traces"),
            None,
            event_bus.clone(),
        ));

        let (base_url, _handle) = start_serve(state).await;

        // Verify the SSE endpoint is available
        let resp = reqwest::get(format!("{base_url}/api/v1/events/stream"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "expected text/event-stream, got: {ct}"
        );

        // Verify the shared event bus is actually connected:
        // subscribing through the bus should see events emitted externally.
        let mut rx = event_bus.subscribe();
        event_bus.emit(thrum_core::event::EventKind::TaskStateChange {
            task_id: TaskId(42),
            repo: RepoName::new("test"),
            from: "pending".into(),
            to: "implementing".into(),
        });
        let event = rx.recv().await.unwrap();
        assert!(matches!(
            event.kind,
            thrum_core::event::EventKind::TaskStateChange { .. }
        ));
    }

    #[tokio::test]
    async fn serve_rest_and_a2a_share_state() {
        // Verify that a task created via REST API is visible through A2A and vice versa.
        // This confirms the shared Arc<Database> is working correctly under --serve.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.redb");
        let shared_db = Arc::new(thrum_db::open_db(&db_path).unwrap());
        let event_bus = EventBus::new();
        let state = Arc::new(ApiState::with_shared_db(
            shared_db,
            dir.path().join("traces"),
            None,
            event_bus,
        ));

        let (base_url, _handle) = start_serve(state).await;
        let client = reqwest::Client::new();

        // Create via REST
        let create_body = serde_json::json!({
            "repo": "cross-check",
            "title": "REST-created task",
            "description": "Should be visible via A2A"
        });
        let resp = client
            .post(format!("{base_url}/api/v1/tasks"))
            .json(&create_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 201);
        let task: serde_json::Value = resp.json().await.unwrap();
        let task_id = task["id"].as_i64().unwrap();

        // Retrieve via A2A GetTask
        let get_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "a2a.GetTask",
            "params": {"task_id": format!("thrum-{task_id}")}
        });
        let resp = client
            .post(format!("{base_url}/a2a"))
            .json(&get_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let rpc_resp: serde_json::Value = resp.json().await.unwrap();
        assert!(rpc_resp["error"].is_null());
        assert_eq!(rpc_resp["result"]["id"], format!("thrum-{task_id}"));
        assert_eq!(rpc_resp["result"]["metadata"]["repo"], "cross-check");
    }
}
