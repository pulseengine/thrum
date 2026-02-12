//! Embedded web dashboard served via HTMX.
//!
//! All HTML templates are compiled into the binary via `include_str!`.
//! The dashboard polls partial endpoints that return HTML fragments,
//! keeping interactivity server-driven with zero JS build step.

use axum::{
    Form, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use std::fmt::Write as FmtWrite;
use std::sync::Arc;
use thrum_core::task::{TaskId, TaskStatus};
use thrum_core::telemetry::{TraceFilter, TraceReader};
use thrum_db::task_store::TaskStore;

use crate::ApiState;

// ─── Embedded Assets ────────────────────────────────────────────────────

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");

// ─── Router ─────────────────────────────────────────────────────────────

/// Build the dashboard sub-router.
///
/// Mount this on the main router alongside the JSON API.
pub fn dashboard_router() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/dashboard", get(index))
        .route("/dashboard/assets/style.css", get(stylesheet))
        .route("/dashboard/partials/status", get(status_partial))
        .route("/dashboard/partials/tasks", get(tasks_partial))
        .route("/dashboard/partials/activity", get(activity_partial))
        .route("/dashboard/tasks/{id}/approve", post(approve_action))
        .route("/dashboard/tasks/{id}/reject", post(reject_action))
}

// ─── Page & Assets ──────────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn stylesheet() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

// ─── Partials ───────────────────────────────────────────────────────────

/// Status counts summary — rendered as a row of cards.
async fn status_partial(
    State(state): State<Arc<ApiState>>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let counts = store.status_counts()?;

    let mut html = String::with_capacity(512);
    html.push_str("<div class=\"status-grid\">");

    // Group counts into meaningful categories
    let pending = counts.get("pending").copied().unwrap_or(0);
    let active = counts.get("claimed").copied().unwrap_or(0)
        + counts.get("implementing").copied().unwrap_or(0)
        + counts.get("reviewing").copied().unwrap_or(0)
        + counts.get("integrating").copied().unwrap_or(0);
    let approval = counts.get("awaiting-approval").copied().unwrap_or(0);
    let merged = counts.get("merged").copied().unwrap_or(0);
    let failed = counts.get("gate1-failed").copied().unwrap_or(0)
        + counts.get("gate2-failed").copied().unwrap_or(0)
        + counts.get("gate3-failed").copied().unwrap_or(0)
        + counts.get("rejected").copied().unwrap_or(0);

    write_card(&mut html, "pending", pending, "Pending");
    write_card(&mut html, "active", active, "Active");
    write_card(&mut html, "approval", approval, "Approval");
    write_card(&mut html, "merged", merged, "Merged");
    write_card(&mut html, "failed", failed, "Failed");

    html.push_str("</div>");
    Ok(Html(html))
}

fn write_card(buf: &mut String, class: &str, count: usize, label: &str) {
    let _ = write!(
        buf,
        "<div class=\"status-card {class}\">\
         <div class=\"count\">{count}</div>\
         <div class=\"label\">{label}</div>\
         </div>",
    );
}

/// Task queue table — full table body with action buttons.
async fn tasks_partial(State(state): State<Arc<ApiState>>) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let tasks = store.list(None, None)?;

    if tasks.is_empty() {
        return Ok(Html("<div class=\"empty\">No tasks in queue</div>".into()));
    }

    let mut html = String::with_capacity(2048);
    html.push_str("<table class=\"task-table\">");
    html.push_str(
        "<thead><tr>\
         <th>ID</th><th>Repo</th><th>Title</th><th>Status</th><th>Retries</th><th>Actions</th>\
         </tr></thead><tbody>",
    );

    for task in &tasks {
        render_task_row_into(&mut html, task);
    }

    html.push_str("</tbody></table>");
    Ok(Html(html))
}

/// Activity log — recent trace events rendered as log lines.
async fn activity_partial(
    State(state): State<Arc<ApiState>>,
) -> Result<Html<String>, DashboardError> {
    let reader = TraceReader::new(&state.trace_dir);
    let filter = TraceFilter {
        limit: Some(30),
        level: None,
        target_prefix: None,
        field_filter: None,
    };

    let events = reader.read_events(&filter).unwrap_or_default();

    if events.is_empty() {
        return Ok(Html(
            "<div class=\"activity-log\">\
             <div class=\"empty\">No activity yet</div>\
             </div>"
                .into(),
        ));
    }

    let mut html = String::with_capacity(2048);
    html.push_str("<div class=\"activity-log\">");

    for event in &events {
        let level = event.level.as_deref().unwrap_or("info");
        let timestamp = event.timestamp.as_deref().unwrap_or("");
        let message = event.message.as_deref().unwrap_or("");
        let target = event.target.as_deref().unwrap_or("");

        // Show only HH:MM:SS portion for readability
        let short_time = if timestamp.len() >= 19 {
            &timestamp[11..19]
        } else {
            timestamp
        };

        let level_lower = level.to_lowercase();
        let time_esc = escape_html(short_time);
        let level_esc = escape_html(&level_lower);
        let msg_esc = escape_html(message);
        let target_esc = escape_html(target);
        let _ = write!(
            html,
            "<div class=\"log-entry\">\
             <span class=\"log-time\">{time_esc}</span>\
             <span class=\"log-level {level_esc}\">{level_esc}</span>\
             <span class=\"log-message\">{msg_esc}</span>\
             <span class=\"log-target\">{target_esc}</span>\
             </div>",
        );
    }

    html.push_str("</div>");
    Ok(Html(html))
}

// ─── Actions ────────────────────────────────────────────────────────────

/// Approve a task and return the updated table row as HTML.
async fn approve_action(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let mut task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;

    if !task.status.needs_human() {
        return Err(DashboardError(format!(
            "task {} is '{}', not awaiting approval",
            id,
            task.status.label()
        )));
    }

    task.status = TaskStatus::Approved;
    task.updated_at = Utc::now();
    store.update(&task)?;

    // Return the updated row
    let mut html = String::new();
    render_task_row_into(&mut html, &task);
    Ok(Html(html))
}

#[derive(serde::Deserialize)]
struct RejectForm {
    feedback: String,
}

/// Reject a task and return the updated table row as HTML.
async fn reject_action(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    Form(form): Form<RejectForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let mut task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;

    task.status = TaskStatus::Rejected {
        feedback: form.feedback,
    };
    task.updated_at = Utc::now();
    store.update(&task)?;

    let mut html = String::new();
    render_task_row_into(&mut html, &task);
    Ok(Html(html))
}

// ─── Helpers ────────────────────────────────────────────────────────────

/// Write a single `<tr>` for a task into the buffer.
fn render_task_row_into(buf: &mut String, task: &thrum_core::task::Task) {
    let id = task.id.0;
    let label = task.status.label();
    let repo = escape_html(&task.repo.to_string());
    let title = escape_html(&task.title);
    let retries = task.retry_count;

    let _ = write!(
        buf,
        "<tr id=\"task-row-{id}\">\
         <td class=\"task-id\">TASK-{id:04}</td>\
         <td>{repo}</td>\
         <td>{title}</td>\
         <td><span class=\"badge badge-{label}\">{label}</span></td>\
         <td>{retries}</td>\
         <td>",
    );

    // Show approve/reject buttons only for AwaitingApproval tasks
    if task.status.needs_human() {
        let _ = write!(
            buf,
            "<div class=\"actions\">\
             <button class=\"btn btn-approve\" \
             hx-post=\"/dashboard/tasks/{id}/approve\" \
             hx-target=\"#task-row-{id}\" \
             hx-swap=\"outerHTML\">Approve</button>\
             <button class=\"btn btn-reject\" \
             onclick=\"openRejectModal({id})\">Reject</button>\
             </div>",
        );
    }

    buf.push_str("</td></tr>");
}

/// Minimal HTML escaping for dynamic content.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ─── Error Type ─────────────────────────────────────────────────────────

struct DashboardError(String);

impl From<anyhow::Error> for DashboardError {
    fn from(e: anyhow::Error) -> Self {
        Self(e.to_string())
    }
}

impl IntoResponse for DashboardError {
    fn into_response(self) -> Response {
        let msg = escape_html(&self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("<div class=\"empty\">Error: {msg}</div>")),
        )
            .into_response()
    }
}
