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
use serde::Deserialize;
use std::fmt::Write as FmtWrite;
use std::sync::Arc;
use thrum_core::repo::ReposConfig;
use thrum_core::task::{CheckResult, GateReport, TaskId, TaskStatus};
use thrum_core::telemetry::{TraceFilter, TraceReader};
use thrum_db::budget_store::BudgetStore;
use thrum_db::memory_store::MemoryStore;
use thrum_db::task_store::TaskStore;

use crate::ApiState;

// ─── Embedded Assets ────────────────────────────────────────────────────

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const LIVE_HTML: &str = include_str!("../assets/live.html");
const LIVE_CSS: &str = include_str!("../assets/live.css");
const REVIEW_HTML: &str = include_str!("../assets/review.html");
const REVIEW_CSS: &str = include_str!("../assets/review.css");

// ─── Router ─────────────────────────────────────────────────────────────

/// Build the dashboard sub-router.
///
/// Mount this on the main router alongside the JSON API.
pub fn dashboard_router() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/dashboard", get(index))
        .route("/dashboard/live", get(live_index))
        .route("/dashboard/assets/style.css", get(stylesheet))
        .route("/dashboard/assets/live.css", get(live_stylesheet))
        .route("/dashboard/assets/review.css", get(review_stylesheet))
        .route("/dashboard/partials/status", get(status_partial))
        .route("/dashboard/partials/tasks", get(tasks_partial))
        .route("/dashboard/partials/activity", get(activity_partial))
        .route("/dashboard/partials/budget", get(budget_partial))
        .route("/dashboard/partials/memory", get(memory_partial))
        .route(
            "/dashboard/partials/task-detail/{id}",
            get(task_detail_partial),
        )
        .route("/dashboard/tasks/create", post(create_task_action))
        .route("/dashboard/tasks/{id}/approve", post(approve_action))
        .route("/dashboard/tasks/{id}/reject", post(reject_action))
        .route("/dashboard/tasks/{id}/edit", post(edit_task_action))
        .route("/dashboard/tasks/{id}/status", post(set_status_action))
        .route("/dashboard/tasks/{id}/delete", post(delete_task_action))
        .route("/dashboard/tasks/{id}/retry", post(retry_task_action))
        .route("/dashboard/tasks/bulk-approve", post(bulk_approve_action))
        .route("/dashboard/tasks/{id}/review", get(review_page))
        .route(
            "/dashboard/tasks/{id}/review/diff",
            get(review_diff_partial),
        )
        .route("/dashboard/memory/clear", post(clear_memory_action))
        .route("/dashboard/memory/decay", post(decay_memory_action))
        .route("/dashboard/budget/update", post(update_budget_action))
        .route("/dashboard/partials/config", get(config_partial))
        .route("/dashboard/a2a/send", post(a2a_send_action))
}

// ─── Page & Assets ──────────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn live_index() -> Html<&'static str> {
    Html(LIVE_HTML)
}

async fn stylesheet() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

async fn live_stylesheet() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        LIVE_CSS,
    )
        .into_response()
}

async fn review_stylesheet() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        REVIEW_CSS,
    )
        .into_response()
}

// ─── Review Page ────────────────────────────────────────────────────────

/// GET /dashboard/tasks/{id}/review — full-page review for approval decisions.
async fn review_page(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;

    // Extract the checkpoint summary from AwaitingApproval status
    let summary = match &task.status {
        TaskStatus::AwaitingApproval { summary } => summary.clone(),
        _ => {
            return Err(DashboardError(format!(
                "task {} is '{}', review page only available for awaiting-approval tasks",
                id,
                task.status.label()
            )));
        }
    };

    // TODO: Load convergence records once convergence_store is implemented
    let _failure_records: Vec<()> = Vec::new();

    // Load memory entries for this task's repo
    let memory_store = MemoryStore::new(db);
    let memories = memory_store
        .query_for_task(&task.repo, 20)
        .unwrap_or_default();

    // Build the review content HTML
    let mut content = String::with_capacity(8192);

    // ── Review Header
    let title_esc = escape_html(&task.title);
    let repo_esc = escape_html(&task.repo.to_string());
    let created = task.created_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let _ = write!(
        content,
        "<div class=\"review-header\">\
         <div class=\"task-title\">TASK-{id:04}: {title_esc}</div>\
         <div class=\"task-meta\">\
         <span><span class=\"badge badge-awaiting-approval\">awaiting-approval</span></span>\
         <span>Repo: <strong>{repo_esc}</strong></span>\
         <span>Retries: <strong>{retries}</strong></span>\
         <span>Created: {created}</span>\
         </div></div>",
        retries = task.retry_count,
    );

    // ── Delta Summary (parsed from diff_summary or show placeholder)
    render_delta_summary(&mut content, &summary.diff_summary);

    // ── Task Description & Acceptance Criteria
    render_description_section(&mut content, &task);

    // ── Agent Reviewer Output
    render_reviewer_section(&mut content, &summary.reviewer_output);

    // ── Diff View (loaded via HTMX from the diff partial endpoint)
    let _ = write!(
        content,
        "<div class=\"review-section\">\
         <div class=\"section-header\" onclick=\"toggleSection('diff-body')\">\
         <h3>Diff</h3>\
         <button class=\"toggle-btn\">&#x25BC;</button>\
         </div>\
         <div class=\"section-body\" id=\"diff-body\">\
         <div class=\"diff-view\" \
              hx-get=\"/dashboard/tasks/{id}/review/diff\" \
              hx-trigger=\"load\" \
              hx-indicator=\"#action-indicator\">\
         <div class=\"diff-empty\">Loading diff...</div>\
         </div></div></div>",
    );

    // ── Gate Reports
    render_gate_reports_section(&mut content, &summary.gate1_report, &summary.gate2_report);

    // ── Memory Context
    render_memory_section(&mut content, &memories);

    // ── Convergence Status (stub until convergence_store exists)
    if task.retry_count > 0 {
        let _ = write!(
            content,
            "<div class=\"review-section\">\
             <div class=\"section-header\">Convergence</div>\
             <div class=\"convergence-info\">Retry count: {}</div>\
             </div>",
            task.retry_count,
        );
    }

    // ── Approve / Reject Actions
    let _ = write!(
        content,
        "<div class=\"review-actions\">\
         <h3>Decision</h3>\
         <div id=\"action-result\"></div>\
         <form id=\"reject-form\" \
               hx-post=\"/dashboard/tasks/{id}/reject\" \
               hx-target=\"#action-result\" \
               hx-swap=\"innerHTML\">\
         <textarea name=\"feedback\" \
                   placeholder=\"Feedback for the agent (required for rejection, optional for approval)...\"></textarea>\
         <div class=\"action-buttons\">\
         <button type=\"button\" class=\"btn btn-approve btn-lg\" id=\"approve-btn\" \
                 hx-post=\"/dashboard/tasks/{id}/approve\" \
                 hx-target=\"#action-result\" \
                 hx-swap=\"innerHTML\" \
                 hx-indicator=\"#action-indicator\">Approve</button>\
         <button type=\"submit\" class=\"btn btn-reject btn-lg\" \
                 hx-indicator=\"#action-indicator\">Reject</button>\
         </div></form></div>",
    );

    // Embed the content into the review template
    let page = REVIEW_HTML.replace("{{REVIEW_CONTENT}}", &content);
    Ok(Html(page))
}

/// GET /dashboard/tasks/{id}/review/diff — HTMX partial returning syntax-colored diff.
async fn review_diff_partial(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;

    if !task.status.is_reviewable() {
        return Ok(Html(
            "<div class=\"diff-empty\">Diff not available for this task status</div>".into(),
        ));
    }

    // Try to fetch the live diff from the git repo
    let diff_text = state
        .config_path
        .as_ref()
        .and_then(|path| thrum_core::repo::ReposConfig::load(path).ok())
        .and_then(|repos_config| repos_config.get(&task.repo).cloned())
        .and_then(|repo_config| thrum_runner::git::GitRepo::open(&repo_config.path).ok())
        .and_then(|git_repo| {
            let branch = task.branch_name();
            git_repo.diff_patch_for_branch(&branch).ok()
        })
        .unwrap_or_default();

    if diff_text.is_empty() {
        return Ok(Html(
            "<div class=\"diff-empty\">No diff available (branch may not exist or repo config not set)</div>".into(),
        ));
    }

    // Render diff with +/- coloring
    let mut html = String::with_capacity(diff_text.len() * 2);
    html.push_str("<pre>");
    for line in diff_text.lines() {
        let escaped = escape_html(line);
        if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("+++")
            || line.starts_with("---")
        {
            let _ = write!(
                html,
                "<span class=\"diff-line diff-line-file\">{escaped}</span>"
            );
        } else if line.starts_with("@@") {
            let _ = write!(
                html,
                "<span class=\"diff-line diff-line-hunk\">{escaped}</span>"
            );
        } else if line.starts_with('+') {
            let _ = write!(
                html,
                "<span class=\"diff-line diff-line-add\">{escaped}</span>"
            );
        } else if line.starts_with('-') {
            let _ = write!(
                html,
                "<span class=\"diff-line diff-line-del\">{escaped}</span>"
            );
        } else {
            let _ = write!(html, "<span class=\"diff-line\">{escaped}</span>");
        }
    }
    html.push_str("</pre>");
    Ok(Html(html))
}

// ─── Review Page Render Helpers ─────────────────────────────────────────

/// Render delta summary statistics (files changed, insertions, deletions).
fn render_delta_summary(buf: &mut String, diff_summary: &str) {
    // Parse counts from diff_summary if available, otherwise show defaults
    let (files, insertions, deletions) = parse_diff_stats(diff_summary);

    let _ = write!(
        buf,
        "<div class=\"delta-summary\">\
         <div class=\"delta-stat files\">\
         <div class=\"stat-value\">{files}</div>\
         <div class=\"stat-label\">Files Changed</div>\
         </div>\
         <div class=\"delta-stat insertions\">\
         <div class=\"stat-value\">+{insertions}</div>\
         <div class=\"stat-label\">Insertions</div>\
         </div>\
         <div class=\"delta-stat deletions\">\
         <div class=\"stat-value\">-{deletions}</div>\
         <div class=\"stat-label\">Deletions</div>\
         </div></div>",
    );
}

/// Parse "X files changed, Y insertions(+), Z deletions(-)" from a diff summary.
fn parse_diff_stats(summary: &str) -> (usize, usize, usize) {
    if summary.is_empty() {
        return (0, 0, 0);
    }

    let mut files = 0usize;
    let mut insertions = 0usize;
    let mut deletions = 0usize;

    for word_pair in summary.split(',') {
        let trimmed = word_pair.trim();
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 2
            && let Ok(n) = parts[0].parse::<usize>()
        {
            if trimmed.contains("file") {
                files = n;
            } else if trimmed.contains("insertion") {
                insertions = n;
            } else if trimmed.contains("deletion") {
                deletions = n;
            }
        }
    }

    (files, insertions, deletions)
}

/// Render the task description and acceptance criteria section.
fn render_description_section(buf: &mut String, task: &thrum_core::task::Task) {
    let desc_esc = escape_html(&task.description);
    buf.push_str(
        "<div class=\"review-section\">\
         <div class=\"section-header\" onclick=\"toggleSection('desc-body')\">\
         <h3>Task Description &amp; Acceptance Criteria</h3>\
         <button class=\"toggle-btn\">&#x25BC;</button>\
         </div><div class=\"section-body\" id=\"desc-body\"><div class=\"review-text\">",
    );

    // Description
    let _ = write!(buf, "<p>{desc_esc}</p>");

    // Acceptance criteria
    if !task.acceptance_criteria.is_empty() {
        buf.push_str(
            "<h4 style=\"margin-top:12px;font-size:12px;color:var(--text-muted);\
                       text-transform:uppercase;letter-spacing:1px;\">Acceptance Criteria</h4>\
                       <ul class=\"criteria-list\">",
        );
        for criterion in &task.acceptance_criteria {
            let c_esc = escape_html(criterion);
            let _ = write!(buf, "<li>{c_esc}</li>");
        }
        buf.push_str("</ul>");
    }

    buf.push_str("</div></div></div>");
}

/// Render the agent reviewer output section.
fn render_reviewer_section(buf: &mut String, reviewer_output: &str) {
    buf.push_str(
        "<div class=\"review-section\">\
         <div class=\"section-header\" onclick=\"toggleSection('reviewer-body')\">\
         <h3>Agent Reviewer Insights</h3>\
         <button class=\"toggle-btn\">&#x25BC;</button>\
         </div><div class=\"section-body\" id=\"reviewer-body\">",
    );

    if reviewer_output.is_empty() {
        buf.push_str("<div class=\"diff-empty\">No reviewer output available</div>");
    } else {
        let output_esc = escape_html(reviewer_output);
        let _ = write!(buf, "<div class=\"reviewer-output\">{output_esc}</div>");
    }

    buf.push_str("</div></div>");
}

/// Render gate reports section with expandable check details.
fn render_gate_reports_section(
    buf: &mut String,
    gate1_report: &GateReport,
    gate2_report: &Option<GateReport>,
) {
    buf.push_str(
        "<div class=\"review-section\">\
         <div class=\"section-header\" onclick=\"toggleSection('gates-body')\">\
         <h3>Gate Reports</h3>\
         <button class=\"toggle-btn\">&#x25BC;</button>\
         </div><div class=\"section-body\" id=\"gates-body\">",
    );

    render_single_gate_report(buf, gate1_report, "g1");

    if let Some(g2) = gate2_report {
        render_single_gate_report(buf, g2, "g2");
    }

    buf.push_str("</div></div>");
}

/// Render a single gate report with its checks.
fn render_single_gate_report(buf: &mut String, report: &GateReport, prefix: &str) {
    let status_class = if report.passed {
        "gate-passed"
    } else {
        "gate-failed"
    };
    let status_icon = if report.passed { "PASSED" } else { "FAILED" };

    let _ = write!(
        buf,
        "<div class=\"gate-report\">\
         <div class=\"gate-header\">\
         <span class=\"gate-label {status_class}\">{level} — {status_icon}</span>\
         <span class=\"gate-duration\">{duration:.1}s</span>\
         </div><ul class=\"check-list\">",
        level = report.level,
        duration = report.duration_secs,
    );

    for (i, check) in report.checks.iter().enumerate() {
        render_check_item(buf, check, prefix, i);
    }

    buf.push_str("</ul></div>");
}

/// Render a single check result item with expandable stdout/stderr.
fn render_check_item(buf: &mut String, check: &CheckResult, prefix: &str, index: usize) {
    let status_class = if check.passed {
        "check-pass"
    } else {
        "check-fail"
    };
    let icon = if check.passed { "&#x2714;" } else { "&#x2718;" };
    let name_esc = escape_html(&check.name);
    let output_id = format!("{prefix}-check-{index}");

    let _ = write!(
        buf,
        "<li class=\"check-item\">\
         <span class=\"check-icon {status_class}\">{icon}</span>\
         <span class=\"check-name {status_class}\">{name_esc}</span>\
         <span style=\"margin-left:8px;font-size:11px;color:var(--text-muted);\">\
         (exit code: {exit_code})</span>",
        exit_code = check.exit_code,
    );

    // Show expandable output if there's stdout or stderr
    let has_stdout = !check.stdout.is_empty();
    let has_stderr = !check.stderr.is_empty();
    if has_stdout || has_stderr {
        let _ = write!(
            buf,
            "<div class=\"check-output\">\
             <button class=\"check-output-toggle\" \
             onclick=\"document.getElementById('{output_id}').classList.toggle('expanded')\">\
             Show output</button>\
             <div class=\"check-output-content\" id=\"{output_id}\">"
        );
        if has_stdout {
            let stdout_esc = escape_html(&check.stdout);
            let _ = write!(buf, "<strong>stdout:</strong>\n{stdout_esc}");
        }
        if has_stdout && has_stderr {
            buf.push_str("\n\n");
        }
        if has_stderr {
            let stderr_esc = escape_html(&check.stderr);
            let _ = write!(buf, "<strong>stderr:</strong>\n{stderr_esc}");
        }
        buf.push_str("</div></div>");
    }

    buf.push_str("</li>");
}

/// Render the memory context section.
fn render_memory_section(buf: &mut String, memories: &[thrum_core::memory::MemoryEntry]) {
    buf.push_str(
        "<div class=\"review-section\">\
         <div class=\"section-header\" onclick=\"toggleSection('memory-body')\">\
         <h3>Memory Context</h3>\
         <button class=\"toggle-btn\">&#x25BC;</button>\
         </div><div class=\"section-body\" id=\"memory-body\">",
    );

    if memories.is_empty() {
        buf.push_str("<div class=\"memory-empty\">No memories recorded for this repo</div>");
    } else {
        buf.push_str("<div class=\"memory-list\">");
        for entry in memories {
            let category_label = entry.category.label();
            let content_esc = escape_html(&entry.content);
            let cat_detail = match &entry.category {
                thrum_core::memory::MemoryCategory::Error { error_type } => escape_html(error_type),
                thrum_core::memory::MemoryCategory::Pattern { pattern_name } => {
                    escape_html(pattern_name)
                }
                thrum_core::memory::MemoryCategory::Decision { alternatives } => {
                    let alts: Vec<String> = alternatives.iter().map(|a| escape_html(a)).collect();
                    alts.join(", ")
                }
                thrum_core::memory::MemoryCategory::Context { scope } => escape_html(scope),
            };
            let _ = write!(
                buf,
                "<div class=\"memory-entry\">\
                 <div class=\"memory-category {category_label}\">\
                 {category_label}: {cat_detail}</div>\
                 <div class=\"memory-content\">{content_esc}</div>\
                 </div>",
            );
        }
        buf.push_str("</div>");
    }

    buf.push_str("</div></div>");
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

/// Task queue table — full table body with action buttons and bulk approve bar.
async fn tasks_partial(State(state): State<Arc<ApiState>>) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let tasks = store.list(None, None)?;

    let mut html = String::with_capacity(4096);

    // Action result banner
    html.push_str("<div id=\"task-action-result\"></div>");

    // Bulk approve bar for awaiting-approval tasks
    let awaiting: Vec<_> = tasks.iter().filter(|t| t.status.needs_human()).collect();
    if !awaiting.is_empty() {
        html.push_str(
            "<form class=\"bulk-bar\" \
             hx-post=\"/dashboard/tasks/bulk-approve\" \
             hx-target=\"#task-action-result\" \
             hx-swap=\"innerHTML\">",
        );
        let _ = write!(
            html,
            "<span class=\"bulk-label\">{} awaiting approval</span>",
            awaiting.len(),
        );
        for t in &awaiting {
            let _ = write!(
                html,
                "<label class=\"bulk-check\">\
                 <input type=\"checkbox\" name=\"task_ids\" value=\"{}\"> TASK-{:04}\
                 </label>",
                t.id.0, t.id.0,
            );
        }
        html.push_str(
            "<button type=\"submit\" class=\"btn btn-approve btn-sm\">Approve Selected</button>\
             </form>",
        );
    }

    if tasks.is_empty() {
        html.push_str("<div class=\"empty\">No tasks in queue</div>");
        return Ok(Html(html));
    }

    html.push_str("<table class=\"task-table\">");
    html.push_str(
        "<thead><tr>\
         <th>ID</th><th>Repo</th><th>Title</th><th>Status</th><th>Timeline</th><th>Retries</th><th>Actions</th>\
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
        // The tracing JSON format nests the message inside `fields.message`.
        // Fall back to extracting it from the fields object.
        let fields_message;
        let message = if let Some(msg) = event.message.as_deref() {
            msg
        } else if let Some(msg) = event.fields.get("message").and_then(|v| v.as_str()) {
            msg
        } else {
            // Build a summary from all fields as last resort
            fields_message = event.fields.to_string();
            fields_message.as_str()
        };
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

// ─── Budget, Memory, Task Detail Partials ───────────────────────────────

/// Budget usage widget — shows spent/remaining/ceiling.
async fn budget_partial(
    State(state): State<Arc<ApiState>>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let budget_store = BudgetStore::new(db);
    let tracker = budget_store
        .load()?
        .unwrap_or_else(|| thrum_core::budget::BudgetTracker::new(1000.0));

    let spent = tracker.total_spent();
    let ceiling = tracker.ceiling_usd;
    let remaining = ceiling - spent;
    let pct = if ceiling > 0.0 {
        (spent / ceiling * 100.0).min(100.0)
    } else {
        0.0
    };

    let fill_class = if pct > 90.0 {
        "budget-bar-fill danger"
    } else if pct > 70.0 {
        "budget-bar-fill warning"
    } else {
        "budget-bar-fill"
    };
    Ok(Html(format!(
        "<div class=\"budget-widget\">\
         <div class=\"budget-header\">\
         <span class=\"budget-label\">Budget</span>\
         <span class=\"budget-numbers\">${spent:.2} / ${ceiling:.2} \
         <span class=\"budget-remaining\">(${remaining:.2} remaining)</span></span>\
         </div>\
         <div class=\"budget-bar\"><div class=\"{fill_class}\" style=\"width:{pct:.0}%\"></div></div>\
         <div id=\"budget-action-result\"></div>\
         <form class=\"budget-controls\" \
               hx-post=\"/dashboard/budget/update\" \
               hx-target=\"#budget-action-result\" \
               hx-swap=\"innerHTML\">\
         <input type=\"number\" name=\"ceiling_usd\" value=\"{ceiling:.0}\" \
                step=\"100\" min=\"0\" class=\"input-sm\" placeholder=\"Ceiling USD\">\
         <label class=\"bulk-check\">\
         <input type=\"checkbox\" name=\"reset_spent\" value=\"on\"> Reset spent\
         </label>\
         <button type=\"submit\" class=\"btn btn-sm\">Update</button>\
         </form>\
         </div>"
    )))
}

/// Memory entries viewer — shows stored memory entries with clear/decay controls.
async fn memory_partial(
    State(state): State<Arc<ApiState>>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = MemoryStore::new(db);
    let entries = store.list_all(None, 50)?;

    let mut html = String::with_capacity(2048);

    // Memory management controls
    html.push_str("<div id=\"memory-action-result\"></div>");
    html.push_str(
        "<div class=\"memory-controls\">\
         <form class=\"inline-form\" \
               hx-post=\"/dashboard/memory/clear\" \
               hx-target=\"#memory-action-result\" \
               hx-swap=\"innerHTML\">\
         <input type=\"text\" name=\"repo\" placeholder=\"Repo filter (empty=all)\" class=\"input-sm\">\
         <button type=\"submit\" class=\"btn btn-reject btn-sm\">Clear</button>\
         </form>\
         <form class=\"inline-form\" \
               hx-post=\"/dashboard/memory/decay\" \
               hx-target=\"#memory-action-result\" \
               hx-swap=\"innerHTML\">\
         <input type=\"number\" name=\"half_life_hours\" value=\"168\" \
                step=\"24\" min=\"1\" class=\"input-sm\" placeholder=\"Half-life (hours)\">\
         <button type=\"submit\" class=\"btn btn-sm\">Decay &amp; Prune</button>\
         </form>\
         </div>",
    );

    html.push_str("<div class=\"memory-list\">");
    if entries.is_empty() {
        html.push_str("<div class=\"empty\">No memory entries</div>");
    } else {
        for entry in &entries {
            let truncated: String = entry.content.chars().take(120).collect();
            let _ = write!(
                html,
                "<div class=\"memory-entry\">\
                 <div class=\"memory-header\">\
                 <span class=\"memory-repo\">{repo}</span>\
                 <span class=\"memory-score\">{score:.2}</span>\
                 </div>\
                 <div class=\"memory-content\">{content}</div>\
                 </div>",
                repo = escape_html(&entry.repo.to_string()),
                score = entry.relevance_score,
                content = escape_html(&truncated),
            );
        }
    }
    html.push_str("</div>");
    Ok(Html(html))
}

/// Task detail partial — shows expanded info for a single task.
async fn task_detail_partial(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);

    let task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;

    let branch = format!(
        "auto/TASK-{:04}/{}/{}",
        task.id.0,
        task.repo,
        task.title.to_lowercase().replace(' ', "-")
    );

    let mut html = String::with_capacity(1024);
    let _ = write!(
        html,
        "<div class=\"task-detail\">\
         <h4>TASK-{:04}: {}</h4>\
         <p>{}</p>\
         <div class=\"branch-name\">{branch}</div>",
        task.id.0,
        escape_html(&task.title),
        escape_html(&task.description),
    );

    if !task.acceptance_criteria.is_empty() {
        html.push_str("<ul class=\"criteria\">");
        for ac in &task.acceptance_criteria {
            let _ = write!(html, "<li>{}</li>", escape_html(ac));
        }
        html.push_str("</ul>");
    }

    html.push_str("</div>");
    Ok(Html(html))
}

// ─── Actions ────────────────────────────────────────────────────────────

/// Approve a task and return a success message (or updated row for dashboard).
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

    // Return a success message (works for both dashboard row swap and review page)
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         TASK-{id:04} approved — moving to integration</div>"
    )))
}

#[derive(serde::Deserialize)]
struct RejectForm {
    feedback: String,
}

/// Reject a task and return a feedback message.
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

    Ok(Html(format!(
        "<div class=\"action-result error\">\
         TASK-{id:04} rejected — returning to implementation with feedback</div>"
    )))
}

// ─── Task Management Actions ─────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateTaskForm {
    repo: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    acceptance_criteria: String,
}

async fn create_task_action(
    State(state): State<Arc<ApiState>>,
    Form(form): Form<CreateTaskForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let repo_name = thrum_core::task::RepoName::new(&form.repo);
    let mut task = thrum_core::task::Task::new(repo_name, form.title, form.description);
    task.acceptance_criteria = form
        .acceptance_criteria
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let task = store.insert(task)?;
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         Created TASK-{:04}: {}</div>",
        task.id.0,
        escape_html(&task.title),
    )))
}

#[derive(Deserialize)]
struct EditTaskForm {
    title: String,
    description: String,
    #[serde(default)]
    acceptance_criteria: String,
}

async fn edit_task_action(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    Form(form): Form<EditTaskForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let mut task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;
    task.title = form.title;
    task.description = form.description;
    task.acceptance_criteria = form
        .acceptance_criteria
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    task.updated_at = Utc::now();
    store.update(&task)?;
    Ok(Html(format!(
        "<div class=\"action-result success\">TASK-{id:04} updated</div>"
    )))
}

#[derive(Deserialize)]
struct SetStatusForm {
    status: String,
}

async fn set_status_action(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    Form(form): Form<SetStatusForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let mut task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;
    task.status = match form.status.as_str() {
        "pending" => TaskStatus::Pending,
        "approved" => TaskStatus::Approved,
        "integrating" => TaskStatus::Integrating,
        "merged" => TaskStatus::Merged {
            commit_sha: "manual".into(),
        },
        "rejected" => TaskStatus::Rejected {
            feedback: "manually rejected from dashboard".into(),
        },
        other => {
            return Ok(Html(format!(
                "<div class=\"action-result error\">Cannot set status to '{}'</div>",
                escape_html(other)
            )));
        }
    };
    task.updated_at = Utc::now();
    store.update(&task)?;
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         TASK-{id:04} status set to {}</div>",
        escape_html(&form.status)
    )))
}

async fn delete_task_action(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let existed = store.delete(&TaskId(id))?;
    if existed {
        Ok(Html(format!(
            "<div class=\"action-result success\">TASK-{id:04} deleted</div>"
        )))
    } else {
        Ok(Html(format!(
            "<div class=\"action-result error\">TASK-{id:04} not found</div>"
        )))
    }
}

async fn retry_task_action(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let mut task = store
        .get(&TaskId(id))?
        .ok_or_else(|| DashboardError(format!("task {id} not found")))?;
    let old_status = task.status.label().to_string();
    task.retry_count = 0;
    task.status = TaskStatus::Pending;
    task.updated_at = Utc::now();
    store.update(&task)?;
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         TASK-{id:04} reset for retry (was {old_status}, retries cleared)</div>"
    )))
}

#[derive(Deserialize)]
struct BulkApproveForm {
    #[serde(default)]
    task_ids: Vec<String>,
}

async fn bulk_approve_action(
    State(state): State<Arc<ApiState>>,
    Form(form): Form<BulkApproveForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let mut approved = 0u32;
    let mut skipped = 0u32;
    for id_str in &form.task_ids {
        if let Ok(id) = id_str.parse::<i64>() {
            if let Ok(Some(mut task)) = store.get(&TaskId(id))
                && task.status.needs_human()
            {
                task.status = TaskStatus::Approved;
                task.updated_at = Utc::now();
                if store.update(&task).is_ok() {
                    approved += 1;
                    continue;
                }
            }
            skipped += 1;
        }
    }
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         Approved {approved} tasks, skipped {skipped}</div>"
    )))
}

// ─── Memory & Budget Actions ─────────────────────────────────────────────

#[derive(Deserialize)]
struct ClearMemoryForm {
    #[serde(default)]
    repo: String,
}

async fn clear_memory_action(
    State(state): State<Arc<ApiState>>,
    Form(form): Form<ClearMemoryForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = MemoryStore::new(db);
    let count = if form.repo.is_empty() {
        store.prune_below(f64::INFINITY)?
    } else {
        let repo_name = thrum_core::task::RepoName::new(&form.repo);
        store.clear_for_repo(&repo_name)?
    };
    let scope = if form.repo.is_empty() {
        "all repos".to_string()
    } else {
        format!("repo '{}'", escape_html(&form.repo))
    };
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         Cleared {count} memory entries for {scope}</div>"
    )))
}

#[derive(Deserialize)]
struct DecayMemoryForm {
    #[serde(default)]
    half_life_hours: String,
}

async fn decay_memory_action(
    State(state): State<Arc<ApiState>>,
    Form(form): Form<DecayMemoryForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = MemoryStore::new(db);
    let half_life: f64 = form.half_life_hours.parse().unwrap_or(168.0);
    let decayed = store.decay_all(half_life)?;
    let pruned = store.prune_below(0.05)?;
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         Decayed {decayed} entries (half-life {half_life:.0}h), pruned {pruned} below threshold</div>"
    )))
}

#[derive(Deserialize)]
struct UpdateBudgetForm {
    #[serde(default)]
    ceiling_usd: String,
    #[serde(default)]
    reset_spent: Option<String>,
}

async fn update_budget_action(
    State(state): State<Arc<ApiState>>,
    Form(form): Form<UpdateBudgetForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let budget_store = BudgetStore::new(db);
    let mut tracker = budget_store
        .load()?
        .unwrap_or_else(|| thrum_core::budget::BudgetTracker::new(1000.0));
    let mut changes = Vec::new();
    if let Ok(ceiling) = form.ceiling_usd.parse::<f64>()
        && (ceiling - tracker.ceiling_usd).abs() > 0.01
    {
        tracker.ceiling_usd = ceiling;
        changes.push(format!("ceiling set to ${ceiling:.2}"));
    }
    if form.reset_spent.as_deref() == Some("on") {
        let old_spent = tracker.total_spent();
        tracker.entries.clear();
        changes.push(format!("reset ${old_spent:.2} spent"));
    }
    budget_store.save(&tracker)?;
    let msg = if changes.is_empty() {
        "No changes made".to_string()
    } else {
        changes.join(", ")
    };
    Ok(Html(format!(
        "<div class=\"action-result success\">Budget updated: {msg}</div>"
    )))
}

// ─── Config & A2A Partials ──────────────────────────────────────────────

async fn config_partial(
    State(state): State<Arc<ApiState>>,
) -> Result<Html<String>, DashboardError> {
    let mut html = String::with_capacity(4096);
    html.push_str("<div class=\"config-panel\">");
    html.push_str("<h3>Repositories</h3>");
    match &state.config_path {
        Some(path) => match ReposConfig::load(path) {
            Ok(config) => {
                html.push_str(
                    "<table class=\"config-table\"><thead><tr>\
                     <th>Name</th><th>Path</th><th>Build</th><th>Test</th><th>Safety</th>\
                     </tr></thead><tbody>",
                );
                for repo in &config.repo {
                    let name_esc = escape_html(&repo.name.to_string());
                    let path_esc = escape_html(&repo.path.display().to_string());
                    let build_esc = escape_html(&repo.build_cmd);
                    let test_esc = escape_html(&repo.test_cmd);
                    let safety = repo
                        .safety_target
                        .as_ref()
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_else(|| "\u{2014}".into());
                    let _ = write!(
                        html,
                        "<tr><td class=\"task-id\">{name_esc}</td><td>{path_esc}</td>\
                         <td><code>{build_esc}</code></td><td><code>{test_esc}</code></td>\
                         <td>{safety}</td></tr>",
                    );
                }
                html.push_str("</tbody></table>");
            }
            Err(e) => {
                let _ = write!(
                    html,
                    "<div class=\"empty\">Failed to load repos config: {}</div>",
                    escape_html(&e.to_string())
                );
            }
        },
        None => {
            html.push_str("<div class=\"empty\">No config path configured</div>");
        }
    }
    html.push_str("<h3 style=\"margin-top:16px;\">Pipeline Configuration</h3>");
    if let Some(pipeline_toml) = state.config_path.as_ref().and_then(|repos_path| {
        let pipeline_path = repos_path.with_file_name("pipeline.toml");
        std::fs::read_to_string(pipeline_path).ok()
    }) {
        let _ = write!(
            html,
            "<pre class=\"config-pre\">{}</pre>",
            escape_html(&pipeline_toml)
        );
    } else {
        html.push_str("<div class=\"empty\">No pipeline.toml found</div>");
    }
    html.push_str("</div>");
    Ok(Html(html))
}

#[derive(Deserialize)]
struct A2aSendForm {
    repo: String,
    message: String,
}

async fn a2a_send_action(
    State(state): State<Arc<ApiState>>,
    Form(form): Form<A2aSendForm>,
) -> Result<Html<String>, DashboardError> {
    let db = state.db();
    let store = TaskStore::new(db);
    let mut lines = form.message.lines();
    let title = lines.next().unwrap_or("Untitled task").to_string();
    let description: String = lines.collect::<Vec<_>>().join("\n");
    let repo_name = thrum_core::task::RepoName::new(&form.repo);
    let task = thrum_core::task::Task::new(repo_name, title, description);
    let task = store.insert(task)?;
    Ok(Html(format!(
        "<div class=\"action-result success\">\
         A2A task created: TASK-{:04} ({})</div>",
        task.id.0,
        escape_html(&task.title),
    )))
}

// ─── Helpers ────────────────────────────────────────────────────────────

/// Render an inline timeline showing pipeline progress as small step indicators.
fn render_inline_timeline(status: &TaskStatus) -> String {
    let stage = match status {
        TaskStatus::Pending => 0,
        TaskStatus::Claimed { .. } => 0,
        TaskStatus::Implementing { .. } => 1,
        TaskStatus::Gate1Failed { .. } => 2,
        TaskStatus::Reviewing { .. } => 3,
        TaskStatus::Gate2Failed { .. } => 4,
        TaskStatus::AwaitingApproval { .. } => 5,
        TaskStatus::Approved => 5,
        TaskStatus::Rejected { .. } => 5,
        TaskStatus::Integrating => 6,
        TaskStatus::Gate3Failed { .. } => 6,
        TaskStatus::Merged { .. } => 7,
    };

    let is_failed = matches!(
        status,
        TaskStatus::Gate1Failed { .. }
            | TaskStatus::Gate2Failed { .. }
            | TaskStatus::Gate3Failed { .. }
            | TaskStatus::Rejected { .. }
    );

    let steps = ["P", "I", "G1", "R", "G2", "A", "Int", "M"];
    let mut out = String::with_capacity(256);
    for (i, &step) in steps.iter().enumerate() {
        let class = if i < stage {
            "timeline-step done"
        } else if i == stage && is_failed {
            "timeline-step failed"
        } else if i == stage {
            "timeline-step active"
        } else {
            "timeline-step"
        };
        let _ = write!(out, "<span class=\"{class}\">{step}</span>");
    }
    out
}

/// Write a single `<tr>` for a task into the buffer.
fn render_task_row_into(buf: &mut String, task: &thrum_core::task::Task) {
    let id = task.id.0;
    let label = task.status.label();
    let repo = escape_html(&task.repo.to_string());
    let title = escape_html(&task.title);
    let retries = task.retry_count;
    let max_retries = thrum_core::task::MAX_RETRIES;
    let exhausted = retries >= max_retries;
    let timeline = render_inline_timeline(&task.status);

    let is_failed = matches!(
        task.status,
        TaskStatus::Gate1Failed { .. }
            | TaskStatus::Gate2Failed { .. }
            | TaskStatus::Gate3Failed { .. }
            | TaskStatus::Rejected { .. }
    );

    let retry_class = if exhausted && is_failed {
        "retry-exhausted"
    } else if retries > 0 {
        "retry-warn"
    } else {
        ""
    };

    let _ = write!(
        buf,
        "<tr id=\"task-row-{id}\" class=\"task-row\">\
         <td class=\"task-id\">TASK-{id:04}</td>\
         <td>{repo}</td>\
         <td>{title}</td>\
         <td><span class=\"badge badge-{label}\">{label}</span></td>\
         <td><div class=\"timeline\">{timeline}</div></td>\
         <td><span class=\"{retry_class}\" title=\"{retries} of {max_retries} retries used\">\
         {retries}/{max_retries}</span></td>\
         <td><div class=\"actions\">",
    );

    // Review link for AwaitingApproval tasks
    if task.status.needs_human() {
        let _ = write!(
            buf,
            "<a href=\"/dashboard/tasks/{id}/review\" class=\"btn btn-approve btn-sm\">Review</a>",
        );
    }

    // Retry button for failed/rejected tasks
    if is_failed {
        let _ = write!(
            buf,
            "<button class=\"btn btn-retry btn-sm\" \
             hx-post=\"/dashboard/tasks/{id}/retry\" \
             hx-target=\"#task-action-result\" \
             hx-swap=\"innerHTML\" \
             title=\"Reset to pending and clear retry count\">\u{21bb} Retry</button>",
        );
    }

    // Status dropdown
    let _ = write!(
        buf,
        "<select class=\"status-select\" name=\"status\" \
         hx-post=\"/dashboard/tasks/{id}/status\" \
         hx-target=\"#task-action-result\" \
         hx-swap=\"innerHTML\" \
         hx-include=\"this\">\
         <option value=\"\" selected disabled>\u{2699}</option>\
         <option value=\"pending\">Reset to Pending</option>\
         <option value=\"approved\">Set Approved</option>\
         <option value=\"merged\">Mark Merged</option>\
         <option value=\"rejected\">Mark Rejected</option>\
         </select>",
    );

    // Delete button
    let _ = write!(
        buf,
        "<button class=\"btn btn-reject btn-sm\" \
         hx-post=\"/dashboard/tasks/{id}/delete\" \
         hx-target=\"#task-action-result\" \
         hx-swap=\"innerHTML\" \
         hx-confirm=\"Delete TASK-{id:04}?\">\u{2715}</button>",
    );

    buf.push_str("</div></td></tr>");
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
