//! Generic CLI agent backend for tools like Vibe, OpenCode, Aider, etc.
//!
//! These are agent-capable tools that run as CLI processes,
//! similar to Claude Code but with different interfaces.
//!
//! Supports session continuation: when `AiRequest::resume_session_id` is set,
//! appends the session flag (e.g., `-s {id}` for OpenCode) to resume context.

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
    /// Flag for session continuation (e.g., "-s" for OpenCode).
    /// When set, `--resume_session_id` causes `{session_flag} {id}` to be appended.
    pub session_flag: Option<String>,
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
            timeout: Duration::from_secs(1200),
            session_flag: None,
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
            timeout: Duration::from_secs(1200),
            session_flag: Some("-s".into()),
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

        let mut cmd = format!("{} {}", self.command, args.join(" "));

        // Session continuation: append session flag if backend supports it
        if let (Some(flag), Some(session_id)) = (&self.session_flag, &request.resume_session_id) {
            cmd.push_str(&format!(" {flag} {session_id}"));
            tracing::info!(
                agent = %self.name,
                session_id = session_id.as_str(),
                "resuming CLI agent session"
            );
        }

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
            session_id: None, // Generic CLI agents don't yet report session IDs
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
