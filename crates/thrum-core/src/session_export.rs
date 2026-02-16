//! Types for session export: capturing full agent conversation transcripts.
//!
//! After an agent completes a task, the session ID can be used to export
//! the full conversation in HTML or Markdown format. Different backends
//! have different export commands:
//! - Claude Code: `claude export <session_id> --format html`
//! - OpenCode: `opencode session export <session_id> --format markdown`

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// Output format for session exports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportFormat {
    /// Markdown (.md) — portable, grep-friendly, diff-friendly.
    #[default]
    Markdown,
    /// HTML (.html) — rich formatting, collapsible tool calls.
    Html,
}

impl ExportFormat {
    /// File extension for this format (without leading dot).
    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Markdown => "md",
            ExportFormat::Html => "html",
        }
    }

    /// MIME type for this format.
    pub fn mime_type(&self) -> &'static str {
        match self {
            ExportFormat::Markdown => "text/markdown",
            ExportFormat::Html => "text/html",
        }
    }
}

impl fmt::Display for ExportFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportFormat::Markdown => write!(f, "markdown"),
            ExportFormat::Html => write!(f, "html"),
        }
    }
}

impl FromStr for ExportFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "markdown" | "md" => Ok(ExportFormat::Markdown),
            "html" => Ok(ExportFormat::Html),
            other => Err(format!(
                "unknown export format '{other}', expected 'markdown' or 'html'"
            )),
        }
    }
}

/// Metadata about a completed session export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    /// Path where the export was written.
    pub path: PathBuf,
    /// Format of the export.
    pub format: ExportFormat,
    /// Size of the exported file in bytes.
    pub size_bytes: u64,
    /// Session ID that was exported.
    pub session_id: String,
    /// Task ID this export belongs to.
    pub task_id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_roundtrip() {
        assert_eq!(
            "markdown".parse::<ExportFormat>().unwrap(),
            ExportFormat::Markdown
        );
        assert_eq!(
            "md".parse::<ExportFormat>().unwrap(),
            ExportFormat::Markdown
        );
        assert_eq!("html".parse::<ExportFormat>().unwrap(), ExportFormat::Html);
        assert_eq!("HTML".parse::<ExportFormat>().unwrap(), ExportFormat::Html);
    }

    #[test]
    fn format_display() {
        assert_eq!(ExportFormat::Markdown.to_string(), "markdown");
        assert_eq!(ExportFormat::Html.to_string(), "html");
    }

    #[test]
    fn format_extension() {
        assert_eq!(ExportFormat::Markdown.extension(), "md");
        assert_eq!(ExportFormat::Html.extension(), "html");
    }

    #[test]
    fn format_unknown_errors() {
        assert!("pdf".parse::<ExportFormat>().is_err());
        assert!("json".parse::<ExportFormat>().is_err());
    }

    #[test]
    fn default_is_markdown() {
        assert_eq!(ExportFormat::default(), ExportFormat::Markdown);
    }
}
