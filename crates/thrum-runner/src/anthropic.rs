//! Anthropic Messages API backend (direct HTTP, no CLI needed).
//!
//! Uses reqwest against `https://api.anthropic.com/v1/messages`.
//! Chat-only: returns text, cannot edit files or run commands.
//! Good for reviews, planning, and headless/CI operation.

use crate::backend::{AiBackend, AiRequest, AiResponse, BackendCapability};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages API backend.
pub struct AnthropicApiBackend {
    client: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl AnthropicApiBackend {
    /// Create from API key and model.
    /// Model examples: "claude-sonnet-4-5-20250929", "claude-opus-4-6", "claude-haiku-4-5-20251001"
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            max_tokens: 4096,
        }
    }

    /// Create from environment variable `ANTHROPIC_API_KEY`.
    pub fn from_env(model: &str) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
        Ok(Self::new(api_key, model.to_string()))
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[async_trait]
impl AiBackend for AnthropicApiBackend {
    fn name(&self) -> &str {
        "anthropic-api"
    }

    fn capability(&self) -> BackendCapability {
        BackendCapability::Chat
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn invoke(&self, request: &AiRequest) -> Result<AiResponse> {
        let max_tokens = request.max_tokens.unwrap_or(self.max_tokens);

        let messages = vec![Message {
            role: "user".into(),
            content: request.prompt.clone(),
        }];

        let body = MessagesRequest {
            model: self.model.clone(),
            max_tokens,
            system: request.system_prompt.clone(),
            messages,
        };

        tracing::info!(
            model = %self.model,
            prompt_len = request.prompt.len(),
            "invoking Anthropic Messages API"
        );

        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to send Anthropic API request")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error ({status}): {body}");
        }

        let resp: MessagesResponse = response
            .json()
            .await
            .context("failed to parse Anthropic response")?;

        let content = resp
            .content
            .iter()
            .filter_map(|block| {
                if block.block_type == "text" {
                    block.text.as_deref()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(AiResponse {
            content,
            model: resp.model,
            input_tokens: Some(resp.usage.input_tokens),
            output_tokens: Some(resp.usage.output_tokens),
            timed_out: false,
            exit_code: None,
            session_id: None,
        })
    }

    async fn health_check(&self) -> Result<()> {
        // Minimal request to check API key validity
        let body = MessagesRequest {
            model: self.model.clone(),
            max_tokens: 1,
            system: None,
            messages: vec![Message {
                role: "user".into(),
                content: "ping".into(),
            }],
        };

        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if response.status().is_success() {
            Ok(())
        } else {
            anyhow::bail!("Anthropic API health check failed: {}", response.status())
        }
    }
}

// ─── API types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<Message>,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct MessagesResponse {
    model: String,
    content: Vec<ContentBlock>,
    usage: Usage,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}
