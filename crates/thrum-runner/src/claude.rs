//! Claude Code CLI backend — the primary agent for implementation tasks.
//!
//! Spawns `claude -p "prompt" --output-format json` as a subprocess.
//! This backend has full agent capabilities: file editing, terminal, git.
//!
//! Supports session continuation: when a previous session ID is provided
//! via `AiRequest::resume_session_id`, uses `--resume {id}` to continue
//! the existing session, preserving agent context across retries.

use crate::backend::{AiBackend, AiRequest, AiResponse, BackendCapability};
use crate::subprocess::{SubprocessOutput, run_cmd, run_cmd_with_sandbox};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default timeout for a Claude session (20 minutes).
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(1200);

/// Claude Code CLI backend.
pub struct ClaudeCliBackend {
    /// Default working directory.
    pub default_cwd: PathBuf,
    /// Session timeout.
    pub timeout: Duration,
    /// Whether to use --dangerously-skip-permissions.
    pub skip_permissions: bool,
}

impl ClaudeCliBackend {
    pub fn new(default_cwd: PathBuf) -> Self {
        Self {
            default_cwd,
            timeout: CLAUDE_TIMEOUT,
            skip_permissions: false,
        }
    }
}

#[async_trait]
impl AiBackend for ClaudeCliBackend {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn capability(&self) -> BackendCapability {
        BackendCapability::Agent
    }

    fn model(&self) -> &str {
        "claude-opus-4-6"
    }

    async fn invoke(&self, request: &AiRequest) -> Result<AiResponse> {
        let cwd = request.cwd.as_deref().unwrap_or(&self.default_cwd);

        let mut cmd_parts = vec!["claude".to_string()];

        // Session continuation: resume an existing session to preserve context
        if let Some(ref session_id) = request.resume_session_id {
            cmd_parts.push("--resume".into());
            cmd_parts.push(session_id.clone());
            tracing::info!(session_id, "resuming Claude session");
        }

        cmd_parts.push("-p".into());

        let escaped = request.prompt.replace('\'', "'\\''");
        cmd_parts.push(format!("'{escaped}'"));
        cmd_parts.push("--output-format".into());
        cmd_parts.push("json".into());

        if self.skip_permissions {
            cmd_parts.push("--dangerously-skip-permissions".into());
        }

        // Write system prompt to temp file if provided
        if let Some(ref sys) = request.system_prompt {
            let tmp =
                std::env::temp_dir().join(format!("thrum-sysprompt-{}.md", std::process::id()));
            tokio::fs::write(&tmp, sys).await?;
            cmd_parts.push("--system-prompt".into());
            cmd_parts.push(format!("'{}'", tmp.display()));
        }

        let cmd = cmd_parts.join(" ");
        tracing::info!(prompt_len = request.prompt.len(), cwd = %cwd.display(), "invoking claude CLI");

        let output =
            run_cmd_with_sandbox(&cmd, cwd, self.timeout, request.sandbox_profile.as_deref())
                .await?;
        let (content, session_id) = parse_claude_output(&output);

        Ok(AiResponse {
            content,
            model: "claude-opus-4-6".into(),
            input_tokens: None,
            output_tokens: None,
            timed_out: output.timed_out,
            exit_code: Some(output.exit_code),
            session_id,
        })
    }

    async fn health_check(&self) -> Result<()> {
        let output = run_cmd(
            "claude --version",
            &self.default_cwd,
            Duration::from_secs(5),
        )
        .await?;
        if output.success() {
            Ok(())
        } else {
            anyhow::bail!("claude CLI not available: {}", output.stderr)
        }
    }
}

/// Parse Claude CLI JSON output, extracting both the result text and session ID.
///
/// Claude Code's `--output-format json` returns a JSON object with:
/// - `result`: the text output from the agent
/// - `session_id`: a unique identifier for the session (used for `--resume`)
fn parse_claude_output(output: &SubprocessOutput) -> (String, Option<String>) {
    if output.timed_out {
        // On timeout, still try to extract session_id from any partial output
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&output.stdout) {
            let session_id = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            return (String::new(), session_id);
        }
        return (String::new(), None);
    }

    // Try JSON parse, fall back to raw stdout
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&output.stdout) {
        let content = json
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or(&output.stdout)
            .to_string();
        let session_id = json
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from);
        (content, session_id)
    } else {
        (output.stdout.clone(), None)
    }
}

/// Load an agent system prompt from a markdown file, optionally embedding
/// a CLAUDE.md from the target repo.
pub async fn load_agent_prompt(agent_file: &Path, claude_md: Option<&Path>) -> Result<String> {
    let mut prompt = tokio::fs::read_to_string(agent_file)
        .await
        .context(format!(
            "failed to read agent file: {}",
            agent_file.display()
        ))?;

    if let Some(claude_md_path) = claude_md {
        let repo_claude = tokio::fs::read_to_string(claude_md_path)
            .await
            .context(format!(
                "failed to read CLAUDE.md: {}",
                claude_md_path.display()
            ))?;
        prompt = prompt.replace("{{CLAUDE_MD}}", &repo_claude);
    }

    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_with_session_id() {
        let output = SubprocessOutput {
            stdout: r#"{"result": "done", "session_id": "ses-abc123"}"#.into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let (content, session_id) = parse_claude_output(&output);
        assert_eq!(content, "done");
        assert_eq!(session_id.as_deref(), Some("ses-abc123"));
    }

    #[test]
    fn parse_json_without_session_id() {
        let output = SubprocessOutput {
            stdout: r#"{"result": "done"}"#.into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let (content, session_id) = parse_claude_output(&output);
        assert_eq!(content, "done");
        assert!(session_id.is_none());
    }

    #[test]
    fn parse_timeout_extracts_session_id() {
        let output = SubprocessOutput {
            stdout: r#"{"result": "partial", "session_id": "ses-timeout"}"#.into(),
            stderr: "timed out".into(),
            exit_code: -1,
            timed_out: true,
        };
        let (content, session_id) = parse_claude_output(&output);
        assert!(content.is_empty());
        assert_eq!(session_id.as_deref(), Some("ses-timeout"));
    }

    #[test]
    fn parse_timeout_no_output() {
        let output = SubprocessOutput {
            stdout: String::new(),
            stderr: "timed out".into(),
            exit_code: -1,
            timed_out: true,
        };
        let (content, session_id) = parse_claude_output(&output);
        assert!(content.is_empty());
        assert!(session_id.is_none());
    }

    #[test]
    fn parse_non_json_output() {
        let output = SubprocessOutput {
            stdout: "raw text output".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };
        let (content, session_id) = parse_claude_output(&output);
        assert_eq!(content, "raw text output");
        assert!(session_id.is_none());
    }

    #[test]
    fn default_timeout_is_1200s() {
        assert_eq!(CLAUDE_TIMEOUT, Duration::from_secs(1200));
    }
}
