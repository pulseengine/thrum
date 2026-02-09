//! Claude Code CLI backend — the primary agent for implementation tasks.
//!
//! Spawns `claude -p "prompt" --output-format json` as a subprocess.
//! This backend has full agent capabilities: file editing, terminal, git.

use crate::backend::{AiBackend, AiRequest, AiResponse, BackendCapability};
use crate::subprocess::{SubprocessOutput, run_cmd};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default timeout for a Claude session (10 minutes).
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(600);

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

        let output = run_cmd(&cmd, cwd, self.timeout).await?;
        let content = parse_claude_output(&output);

        Ok(AiResponse {
            content,
            model: "claude-opus-4-6".into(),
            input_tokens: None,
            output_tokens: None,
            timed_out: output.timed_out,
            exit_code: Some(output.exit_code),
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

fn parse_claude_output(output: &SubprocessOutput) -> String {
    if output.timed_out {
        return String::new();
    }
    // Try JSON parse, fall back to raw stdout
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&output.stdout) {
        json.get("result")
            .and_then(|v| v.as_str())
            .unwrap_or(&output.stdout)
            .to_string()
    } else {
        output.stdout.clone()
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
