//! Generic CLI agent backend for tools like Vibe, OpenCode, Aider, etc.
//!
//! These are agent-capable tools that run as CLI processes,
//! similar to Claude Code but with different interfaces.

use crate::backend::{AiBackend, AiRequest, AiResponse, BackendCapability};
use crate::subprocess::run_cmd;
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;

/// A generic CLI-based AI agent.
pub struct CliAgentBackend {
    /// Display name (e.g., "vibe", "opencode", "aider").
    pub name: String,
    /// The CLI command to invoke (e.g., "vibe", "opencode").
    pub command: String,
    /// How to pass the prompt (e.g., ["-m", "{prompt}"] or ["{prompt}"]).
    /// Use `{prompt}` as placeholder for the actual prompt text.
    pub prompt_args: Vec<String>,
    /// Model name this tool uses.
    pub model_name: String,
    /// Default working directory.
    pub default_cwd: PathBuf,
    /// Session timeout.
    pub timeout: Duration,
}

impl CliAgentBackend {
    /// Create a Vibe backend.
    pub fn vibe(default_cwd: PathBuf) -> Self {
        Self {
            name: "vibe".into(),
            command: "vibe".into(),
            prompt_args: vec!["-m".into(), "{prompt}".into()],
            model_name: "devstral-small-2505".into(),
            default_cwd,
            timeout: Duration::from_secs(600),
        }
    }

    /// Create an OpenCode backend.
    pub fn opencode(default_cwd: PathBuf) -> Self {
        Self {
            name: "opencode".into(),
            command: "opencode".into(),
            prompt_args: vec!["-m".into(), "{prompt}".into()],
            model_name: "devstral-small-2505".into(),
            default_cwd,
            timeout: Duration::from_secs(600),
        }
    }
}

#[async_trait]
impl AiBackend for CliAgentBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn capability(&self) -> BackendCapability {
        BackendCapability::Agent
    }

    fn model(&self) -> &str {
        &self.model_name
    }

    async fn invoke(&self, request: &AiRequest) -> Result<AiResponse> {
        let cwd = request.cwd.as_deref().unwrap_or(&self.default_cwd);

        // Build command with prompt substitution
        let escaped = request.prompt.replace('\'', "'\\''");
        let args: Vec<String> = self
            .prompt_args
            .iter()
            .map(|a| a.replace("{prompt}", &format!("'{escaped}'")))
            .collect();

        let cmd = format!("{} {}", self.command, args.join(" "));

        tracing::info!(
            agent = %self.name,
            prompt_len = request.prompt.len(),
            cwd = %cwd.display(),
            "invoking CLI agent"
        );

        let output = run_cmd(&cmd, cwd, self.timeout).await?;

        Ok(AiResponse {
            content: output.stdout,
            model: self.model_name.clone(),
            input_tokens: None,
            output_tokens: None,
            timed_out: output.timed_out,
            exit_code: Some(output.exit_code),
        })
    }

    async fn health_check(&self) -> Result<()> {
        let output = run_cmd(
            &format!("{} --version", self.command),
            &self.default_cwd,
            Duration::from_secs(5),
        )
        .await?;

        if output.success() {
            Ok(())
        } else {
            anyhow::bail!("{} CLI not available", self.name)
        }
    }
}
