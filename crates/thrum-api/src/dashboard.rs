//! Unified web dashboard served via HTMX + SSE.
//!
//! All HTML templates are compiled into the binary via `include_str!`.
//! The dashboard uses HTMX polling for DB-backed partials and native
//! EventSource for real-time agent activity and event streaming.
//! Zero JS build step — all interactivity is server-driven or inline.

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::Deserialize;
use std::fmt::Write as FmtWrite;
use std::sync::Arc;
use thrum_core::task::{TaskId, TaskStatus};
use thrum_core::telemetry::{TraceFilter, TraceReader};
use thrum_db::budget_store::BudgetStore;
use thrum_db::gate_store::GateStore;
use thrum_db::memory_store::MemoryStore;
use thrum_db::task_store::TaskStore;

use crate::ApiState;

// ─── Embedded Assets ────────────────────────────────────────────────────

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");

// ─── Router ─────────────────────────────────────────────────────────────

/// Build the dashboard sub-router.
///
/// Mount this on the main router alongside the JSON API.
/// The unified dashboard merges the previous polling-based `/dashboard`
/// and SSE-based `/dashboard/live` into a single observability page.
pub fn dashboard_router() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/dashboard", get(index))
        .route("/dashboard/assets/style.css", get(stylesheet))
        // HTMX partials — polled every 5s for DB-backed data
        .route("/dashboard/partials/status", get(status_partial))
        .route("/dashboard/partials/tasks", get(tasks_partial))
        .route("/dashboard/partials/activity", get(activity_partial))
        .route("/dashboard/partials/budget", get(budget_partial))
        .route("/dashboard/partials/memory", get(memory_partial))
        // Task detail — loaded on-demand when a row is expanded
        .route(
            "/dashboard/partials/task-detail/{id}",
            get(task_detail_partial),
        )
        // Actions
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

/// Budget usage bar — shows spent / remaining / ceiling.
async fn budget_partial(
    State(state): State<Arc<ApiState>>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let budget_store = BudgetStore::new(db);

    let mut html = String::with_capacity(512);

    match budget_store.load() {
        Ok(Some(tracker)) => {
            let spent = tracker.total_spent();
            let ceiling = tracker.ceiling_usd;
            let remaining = tracker.remaining();
            let pct = if ceiling > 0.0 {
                (spent / ceiling * 100.0).min(100.0)
            } else {
                0.0
            };

            let bar_class = if pct > 90.0 {
                "budget-bar-fill danger"
            } else if pct > 70.0 {
                "budget-bar-fill warning"
            } else {
                "budget-bar-fill"
            };

            let _ = write!(
                html,
                "<div class=\"budget-widget\">\
                 <div class=\"budget-header\">\
                 <span class=\"budget-label\">Budget</span>\
                 <span class=\"budget-numbers\">\
                 ${spent:.2} / ${ceiling:.2} \
                 <span class=\"budget-remaining\">(${remaining:.2} remaining)</span>\
                 </span>\
                 </div>\
                 <div class=\"budget-bar\">\
                 <div class=\"{bar_class}\" style=\"width:{pct:.1}%\"></div>\
                 </div>\
                 </div>",
            );
        }
        _ => {
            html.push_str(
                "<div class=\"budget-widget\">\
                 <div class=\"budget-header\">\
                 <span class=\"budget-label\">Budget</span>\
                 <span class=\"budget-numbers\">Not configured</span>\
                 </div>\
                 </div>",
            );
        }
    }

    Ok(Html(html))
}

/// Task queue table — clickable rows that expand to show detail.
async fn tasks_partial(State(state): State<Arc<ApiState>>) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let tasks = store.list(None, None)?;

    if tasks.is_empty() {
        return Ok(Html("<div class=\"empty\">No tasks in queue</div>".into()));
    }

    let mut html = String::with_capacity(4096);
    html.push_str("<table class=\"task-table\">");
    html.push_str(
        "<thead><tr>\
         <th></th><th>ID</th><th>Repo</th><th>Title</th>\
         <th>Status</th><th>Retries</th><th>Timeline</th><th>Actions</th>\
         </tr></thead><tbody>",
    );

    for task in &tasks {
        render_task_row_into(&mut html, task);
    }

    html.push_str("</tbody></table>");
    Ok(Html(html))
}

/// Per-task detail view — loaded via HTMX when a task row is expanded.
/// Shows description, acceptance criteria, branch, gate reports, convergence, and diff.
async fn task_detail_partial(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let task_store = TaskStore::new(db);
    let gate_store = GateStore::new(db);
    let task = task_store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;

    let mut html = String::with_capacity(4096);
    html.push_str("<div class=\"task-detail\">");

    // ── Description & Acceptance Criteria ──
    html.push_str("<div class=\"detail-section\">");
    html.push_str("<h4>Description</h4>");
    let desc_esc = escape_html(&task.description);
    let _ = write!(html, "<pre class=\"detail-pre\">{desc_esc}</pre>");

    if !task.acceptance_criteria.is_empty() {
        html.push_str("<h4>Acceptance Criteria</h4><ul class=\"criteria-list\">");
        for criterion in &task.acceptance_criteria {
            let c_esc = escape_html(criterion);
            let _ = write!(html, "<li>{c_esc}</li>");
        }
        html.push_str("</ul>");
    }
    html.push_str("</div>");

    // ── Branch ──
    let branch = task.branch_name();
    let branch_esc = escape_html(&branch);
    let _ = write!(
        html,
        "<div class=\"detail-section\">\
         <h4>Branch</h4>\
         <code class=\"branch-name\">{branch_esc}</code>\
         </div>"
    );

    // ── State Timeline ──
    html.push_str("<div class=\"detail-section\">");
    html.push_str("<h4>State Timeline</h4>");
    render_state_timeline(&mut html, &task);
    html.push_str("</div>");

    // ── Gate Reports ──
    let gate_reports = gate_store.get_all_for_task(&TaskId(id))?;
    if !gate_reports.is_empty() {
        html.push_str("<div class=\"detail-section\"><h4>Gate Reports</h4>");
        for report in &gate_reports {
            let gate_label = escape_html(&format!("{}", report.level));
            let status_class = if report.passed { "pass" } else { "fail" };
            let status_text = if report.passed { "PASS" } else { "FAIL" };
            let _ = write!(
                html,
                "<div class=\"gate-report\">\
                 <div class=\"gate-header\">\
                 <span class=\"gate-name\">{gate_label}</span>\
                 <span class=\"gate-status {status_class}\">{status_text}</span>\
                 <span class=\"gate-duration\">{:.1}s</span>\
                 </div>",
                report.duration_secs,
            );

            for check in &report.checks {
                let check_class = if check.passed { "pass" } else { "fail" };
                let check_icon = if check.passed { "+" } else { "x" };
                let check_name = escape_html(&check.name);
                let _ = write!(
                    html,
                    "<div class=\"gate-check {check_class}\">\
                     <span class=\"check-icon\">{check_icon}</span>\
                     <span class=\"check-name\">{check_name}</span>\
                     </div>",
                );

                // Show stderr for failed checks (truncated)
                if !check.passed && !check.stderr.is_empty() {
                    let stderr_trunc = if check.stderr.len() > 500 {
                        format!("{}...(truncated)", &check.stderr[..500])
                    } else {
                        check.stderr.clone()
                    };
                    let stderr_esc = escape_html(&stderr_trunc);
                    let _ = write!(html, "<pre class=\"check-output\">{stderr_esc}</pre>",);
                }
            }
            html.push_str("</div>");
        }
        html.push_str("</div>");
    }

    // TODO: Add convergence / retry history once convergence_store exists

    // ── Diff (for reviewable tasks) ──
    if task.status.is_reviewable() {
        let _ = write!(
            html,
            "<div class=\"detail-section\">\
             <h4>Diff</h4>\
             <div class=\"diff-container\" \
             hx-get=\"/api/v1/tasks/{id}/diff\" \
             hx-trigger=\"load\" \
             hx-swap=\"innerHTML\">\
             <div class=\"empty\">Loading diff&hellip;</div>\
             </div>\
             </div>",
        );
    }

    html.push_str("</div>");
    Ok(Html(html))
}

/// Memory entries viewer — filterable by repo.
#[derive(Deserialize)]
struct MemoryQuery {
    repo: Option<String>,
}

async fn memory_partial(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<MemoryQuery>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let memory_store = MemoryStore::new(db);
    let task_store = TaskStore::new(db);

    // Collect unique repos for the filter dropdown
    let tasks = task_store.list(None, None)?;
    let mut repos: Vec<String> = tasks.iter().map(|t| t.repo.to_string()).collect();
    repos.sort();
    repos.dedup();

    let repo_filter = query
        .repo
        .as_deref()
        .filter(|r| !r.is_empty())
        .map(thrum_core::task::RepoName::new);

    let entries = memory_store.list_all(repo_filter.as_ref(), 50)?;

    let mut html = String::with_capacity(2048);

    // Repo filter form
    let selected_repo = query.repo.as_deref().unwrap_or("");
    html.push_str(
        "<div class=\"memory-filter\">\
         <select name=\"repo\" \
         hx-get=\"/dashboard/partials/memory\" \
         hx-target=\"#memory-section\" \
         hx-swap=\"innerHTML\" \
         hx-include=\"this\">",
    );
    let all_selected = if selected_repo.is_empty() {
        " selected"
    } else {
        ""
    };
    let _ = write!(html, "<option value=\"\"{all_selected}>All repos</option>");
    for repo in &repos {
        let repo_esc = escape_html(repo);
        let sel = if repo.as_str() == selected_repo {
            " selected"
        } else {
            ""
        };
        let _ = write!(
            html,
            "<option value=\"{repo_esc}\"{sel}>{repo_esc}</option>"
        );
    }
    html.push_str("</select></div>");

    if entries.is_empty() {
        html.push_str("<div class=\"empty\">No memory entries</div>");
    } else {
        html.push_str("<div class=\"memory-list\">");
        for entry in &entries {
            let cat_label = entry.category.label();
            let cat_class = cat_label;
            let repo_esc = escape_html(&entry.repo.to_string());
            let content_esc = escape_html(&entry.content);
            let score = entry.relevance_score;
            let access = entry.access_count;

            let _ = write!(
                html,
                "<div class=\"memory-entry\">\
                 <div class=\"memory-header\">\
                 <span class=\"memory-category badge badge-{cat_class}\">{cat_label}</span>\
                 <span class=\"memory-repo\">{repo_esc}</span>\
                 <span class=\"memory-score\" title=\"Relevance score\">{score:.2}</span>\
                 <span class=\"memory-access\" title=\"Access count\">x{access}</span>\
                 </div>\
                 <div class=\"memory-content\">{content_esc}</div>\
                 </div>",
            );
        }
        html.push_str("</div>");
    }

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
/// Rows are clickable — expanding to reveal a detail panel loaded via HTMX.
fn render_task_row_into(buf: &mut String, task: &thrum_core::task::Task) {
    let id = task.id.0;
    let label = task.status.label();
    let repo = escape_html(&task.repo.to_string());
    let title = escape_html(&task.title);
    let retries = task.retry_count;
    let detail_id = format!("task-detail-{id}");

    // Main row — clickable to toggle detail
    let _ = write!(
        buf,
        "<tr id=\"task-row-{id}\" class=\"task-row\" \
         hx-get=\"/dashboard/partials/task-detail/{id}\" \
         hx-target=\"#{detail_id}\" \
         hx-swap=\"innerHTML\" \
         hx-trigger=\"click\" \
         hx-select=\".task-detail\" \
         role=\"button\" tabindex=\"0\">\
         <td class=\"expand-icon\">&#9654;</td>\
         <td class=\"task-id\">TASK-{id:04}</td>\
         <td>{repo}</td>\
         <td class=\"task-title-cell\">{title}</td>\
         <td><span class=\"badge badge-{label}\">{label}</span></td>\
         <td>{retries}</td>\
         <td>",
    );

    // Inline timeline bar
    render_inline_timeline(buf, task);

    buf.push_str("</td><td>");

    // Approve/reject buttons
    if task.status.needs_human() {
        let _ = write!(
            buf,
            "<div class=\"actions\" onclick=\"event.stopPropagation()\">\
             <button class=\"btn btn-approve\" \
             hx-post=\"/dashboard/tasks/{id}/approve\" \
             hx-target=\"#task-row-{id}\" \
             hx-swap=\"outerHTML\">Approve</button>\
             <button class=\"btn btn-reject\" \
             onclick=\"openRejectModal({id})\">Reject</button>\
             </div>",
        );
    }

    // Close main row, then add the hidden detail row
    let _ = write!(
        buf,
        "</td></tr>\
         <tr class=\"detail-row\">\
         <td colspan=\"8\" id=\"{detail_id}\" class=\"detail-cell\"></td>\
         </tr>",
    );
}

/// Render a compact inline timeline showing state progression.
fn render_inline_timeline(buf: &mut String, task: &thrum_core::task::Task) {
    // Define the pipeline stages in order
    let stages: &[(&str, &str)] = &[
        ("pending", "P"),
        ("implementing", "I"),
        ("gate1", "G1"),
        ("reviewing", "R"),
        ("gate2", "G2"),
        ("approval", "A"),
        ("integrating", "Int"),
        ("gate3", "G3"),
        ("merged", "M"),
    ];

    let current = task.status.label();
    let current_idx = match current {
        "pending" => 0,
        "claimed" | "implementing" => 1,
        "gate1-failed" => 2,
        "reviewing" => 3,
        "gate2-failed" => 4,
        "awaiting-approval" => 5,
        "approved" | "integrating" => 6,
        "gate3-failed" => 7,
        "merged" => 8,
        "rejected" => 5, // rejected from approval
        _ => 0,
    };

    let is_failed = current.contains("failed") || current == "rejected";

    buf.push_str("<div class=\"timeline\">");
    for (i, (_, abbr)) in stages.iter().enumerate() {
        let class = if i < current_idx {
            "timeline-step done"
        } else if i == current_idx {
            if is_failed {
                "timeline-step failed"
            } else {
                "timeline-step active"
            }
        } else {
            "timeline-step"
        };
        let _ = write!(buf, "<span class=\"{class}\">{abbr}</span>");
    }
    buf.push_str("</div>");
}

/// Render a detailed state timeline for the task detail view.
fn render_state_timeline(buf: &mut String, task: &thrum_core::task::Task) {
    let label = task.status.label();
    let created = task.created_at.format("%Y-%m-%d %H:%M:%S UTC");
    let updated = task.updated_at.format("%Y-%m-%d %H:%M:%S UTC");

    let _ = write!(
        buf,
        "<div class=\"state-timeline\">\
         <div class=\"timeline-entry\">\
         <span class=\"timeline-dot done\"></span>\
         <span class=\"timeline-label\">Created</span>\
         <span class=\"timeline-time\">{created}</span>\
         </div>"
    );

    // Show current state
    let dot_class = if label.contains("failed") || label == "rejected" {
        "timeline-dot failed"
    } else if label == "merged" {
        "timeline-dot done"
    } else {
        "timeline-dot active"
    };
    let label_esc = escape_html(label);
    let _ = write!(
        buf,
        "<div class=\"timeline-entry\">\
         <span class=\"{dot_class}\"></span>\
         <span class=\"timeline-label\">{label_esc}</span>\
         <span class=\"timeline-time\">{updated}</span>\
         </div>"
    );

    if task.retry_count > 0 {
        let _ = write!(
            buf,
            "<div class=\"timeline-entry\">\
             <span class=\"timeline-dot\"></span>\
             <span class=\"timeline-label\">Retries: {}</span>\
             </div>",
            task.retry_count,
        );
    }

    buf.push_str("</div>");
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
