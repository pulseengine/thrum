//! Session export: capture full agent conversation transcripts.
//!
//! After a task completes, the stored session ID can be used to export
//! the full agent conversation in HTML or Markdown format. This module
//! handles the CLI invocation and file management for exports.
//!
//! Supports multiple backends:
//! - **Claude Code**: `claude export <session_id> --format {html|markdown}`
//! - **OpenCode**: `opencode session export <session_id> --format markdown`
//!
//! Exports are stored in the `traces/` directory alongside OTEL trace files,
//! named as `export-TASK-XXXX-{timestamp}.{ext}`.

use crate::subprocess::run_cmd;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thrum_core::session_export::{ExportFormat, ExportResult};

/// Timeout for export commands (2 minutes should be more than enough).
const EXPORT_TIMEOUT: Duration = Duration::from_secs(120);

/// Export a session transcript and write it to the traces directory.
///
/// Determines the correct CLI command based on the backend name,
/// invokes it, and writes the output to a file in `trace_dir`.
///
/// Returns the export result with path and metadata, or an error
/// if the export command fails or the session ID is invalid.
pub async fn export_session(
    session_id: &str,
    task_id: i64,
    format: ExportFormat,
    trace_dir: &Path,
    backend_name: Option<&str>,
) -> Result<ExportResult> {
    // Ensure the trace directory exists
    std::fs::create_dir_all(trace_dir).context(format!(
        "failed to create trace dir: {}",
        trace_dir.display()
    ))?;

    // Build the export command based on backend
    let cmd = build_export_command(session_id, format, backend_name);
    tracing::info!(
        session_id,
        task_id,
        format = %format,
        backend = backend_name.unwrap_or("auto"),
        cmd = %cmd,
        "exporting session transcript"
    );

    // Run the export command from the current directory
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let output = run_cmd(&cmd, &cwd, EXPORT_TIMEOUT).await?;

    if !output.success() {
        anyhow::bail!(
            "export command failed (exit {}): {}\nstderr: {}",
            output.exit_code,
            cmd,
            output.stderr
        );
    }

    if output.stdout.trim().is_empty() {
        anyhow::bail!(
            "export command produced empty output for session '{session_id}'. \
             The session may not exist or may have expired."
        );
    }

    // Write the export to a file
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
    let filename = format!(
        "export-TASK-{task_id:04}-{timestamp}.{}",
        format.extension()
    );
    let export_path = trace_dir.join(&filename);

    std::fs::write(&export_path, &output.stdout).context(format!(
        "failed to write export to {}",
        export_path.display()
    ))?;

    let size_bytes = output.stdout.len() as u64;
    tracing::info!(
        path = %export_path.display(),
        size_bytes,
        "session export written"
    );

    Ok(ExportResult {
        path: export_path,
        format,
        size_bytes,
        session_id: session_id.to_string(),
        task_id,
    })
}

/// Build the CLI command string for exporting a session transcript.
///
/// Selects the command based on the backend name:
/// - `"claude-code"` or `None` (default): uses `claude export`
/// - `"opencode"`: uses `opencode session export`
/// - Others: falls back to `claude export`
fn build_export_command(
    session_id: &str,
    format: ExportFormat,
    backend_name: Option<&str>,
) -> String {
    let format_str = format.to_string();

    match backend_name {
        Some("opencode") => {
            format!("opencode session export {session_id} --format {format_str}")
        }
        // Claude Code is the default backend
        Some("claude-code") | None | Some(_) => {
            format!("claude export {session_id} --format {format_str}")
        }
    }
}

/// List existing session exports in the trace directory for a given task.
pub fn list_exports(trace_dir: &Path, task_id: i64) -> Result<Vec<PathBuf>> {
    let prefix = format!("export-TASK-{task_id:04}-");
    let mut exports = Vec::new();

    if !trace_dir.exists() {
        return Ok(exports);
    }

    for entry in std::fs::read_dir(trace_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(&prefix) {
            exports.push(entry.path());
        }
    }

    exports.sort();
    Ok(exports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_claude_export_markdown() {
        let cmd = build_export_command("ses-abc123", ExportFormat::Markdown, None);
        assert_eq!(cmd, "claude export ses-abc123 --format markdown");
    }

    #[test]
    fn build_claude_export_html() {
        let cmd = build_export_command("ses-abc123", ExportFormat::Html, Some("claude-code"));
        assert_eq!(cmd, "claude export ses-abc123 --format html");
    }

    #[test]
    fn build_opencode_export() {
        let cmd = build_export_command("ses-xyz789", ExportFormat::Markdown, Some("opencode"));
        assert_eq!(cmd, "opencode session export ses-xyz789 --format markdown");
    }

    #[test]
    fn build_unknown_backend_falls_back_to_claude() {
        let cmd = build_export_command("ses-123", ExportFormat::Html, Some("some-new-agent"));
        assert_eq!(cmd, "claude export ses-123 --format html");
    }

    #[test]
    fn list_exports_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let exports = list_exports(dir.path(), 42).unwrap();
        assert!(exports.is_empty());
    }

    #[test]
    fn list_exports_nonexistent_dir() {
        let exports = list_exports(Path::new("/nonexistent/path"), 42).unwrap();
        assert!(exports.is_empty());
    }

    #[test]
    fn list_exports_finds_matching_files() {
        let dir = tempfile::tempdir().unwrap();

        // Create some export files
        std::fs::write(
            dir.path().join("export-TASK-0042-20260214-120000.md"),
            "# Export",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("export-TASK-0042-20260214-130000.html"),
            "<html>Export</html>",
        )
        .unwrap();
        // Different task — should not match
        std::fs::write(
            dir.path().join("export-TASK-0001-20260214-120000.md"),
            "# Other",
        )
        .unwrap();
        // OTEL file — should not match
        std::fs::write(
            dir.path().join("spans-2026-02-14.jsonl"),
            "{\"trace\":true}",
        )
        .unwrap();

        let exports = list_exports(dir.path(), 42).unwrap();
        assert_eq!(exports.len(), 2);
        assert!(
            exports[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("export-TASK-0042-")
        );
    }
}
