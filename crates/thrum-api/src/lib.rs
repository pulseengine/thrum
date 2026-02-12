//! HTTP API for thrum, enabling external integrations
//! (OpenClawed, Telegram bots, CI/CD webhooks).
//!
//! Built with axum for async HTTP serving.

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
use thrum_runner::git::GitRepo;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

/// Shared application state for API handlers.
pub struct ApiState {
    pub db_path: PathBuf,
    pub trace_dir: PathBuf,
    /// Path to repos.toml — needed for diff endpoint to locate repo working dirs.
    pub config_path: Option<PathBuf>,
}

impl ApiState {
    /// Open the database (creates if needed).
    fn db(&self) -> Result<redb::Database, AppError> {
        thrum_db::open_db(&self.db_path).map_err(AppError::Internal)
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

/// Build the axum router with all API routes.
pub fn api_router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health_check))
        .route("/api/v1/status", get(status))
        .route("/api/v1/tasks", get(list_tasks).post(create_task))
        .route("/api/v1/tasks/{id}", get(get_task))
        .route("/api/v1/tasks/{id}/diff", get(get_task_diff))
        .route("/api/v1/tasks/{id}/approve", post(approve_task))
        .route("/api/v1/tasks/{id}/reject", post(reject_task))
        .route("/api/v1/traces", get(list_traces))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the API server.
pub async fn serve(state: Arc<ApiState>, bind_addr: &str) -> anyhow::Result<()> {
    let app = api_router(state);
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "starting API server");
    axum::serve(listener, app).await?;
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
}

async fn status(State(state): State<Arc<ApiState>>) -> Result<Json<StatusResponse>, AppError> {
    let db = state.db()?;
    let store = TaskStore::new(&db);
    let counts = store.status_counts()?;

    Ok(Json(StatusResponse {
        healthy: true,
        task_counts: counts,
        db_path: state.db_path.display().to_string(),
        trace_dir: state.trace_dir.display().to_string(),
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
    let db = state.db()?;
    let store = TaskStore::new(&db);

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
    let db = state.db()?;
    let store = TaskStore::new(&db);

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

    let db = state.db()?;
    let store = TaskStore::new(&db);

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
    let db = state.db()?;
    let store = TaskStore::new(&db);

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
    let db = state.db()?;
    let store = TaskStore::new(&db);

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
    let db = state.db()?;
    let store = TaskStore::new(&db);

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
        let _db = thrum_db::open_db(&db_path).unwrap();
        let state = Arc::new(ApiState {
            db_path,
            trace_dir: dir.path().join("traces"),
            config_path: None,
        });
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
            let db = state.db().unwrap();
            let store = TaskStore::new(&db);
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
}
