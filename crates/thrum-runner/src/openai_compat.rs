//! OpenAI-compatible API backend for Mistral/Devstral2 and other providers.
//!
//! Uses `async-openai` pointed at any OpenAI-compatible endpoint.
//! Chat-only: returns text, cannot edit files or run commands.
//!
//! Supported providers:
//! - Mistral AI (Devstral2): `https://api.mistral.ai/v1`
//! - OpenAI: `https://api.openai.com/v1`
//! - Any OpenAI-compatible server (vLLM, Ollama, etc.)

use crate::backend::{AiBackend, AiRequest, AiResponse, BackendCapability};
use anyhow::{Context, Result};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestUserMessage, CreateChatCompletionRequestArgs,
};
use async_trait::async_trait;

/// Well-known provider presets.
pub enum Provider {
    /// Mistral AI (Devstral2, Codestral, etc.)
    Mistral,
    /// OpenAI (GPT-4o, etc.)
    OpenAi,
    /// Custom endpoint.
    Custom { base_url: String },
}

impl Provider {
    fn base_url(&self) -> &str {
        match self {
            Provider::Mistral => "https://api.mistral.ai/v1",
            Provider::OpenAi => "https://api.openai.com/v1",
            Provider::Custom { base_url } => base_url,
        }
    }

    fn env_key_name(&self) -> &str {
        match self {
            Provider::Mistral => "MISTRAL_API_KEY",
            Provider::OpenAi => "OPENAI_API_KEY",
            Provider::Custom { .. } => "OPENAI_API_KEY",
        }
    }
}

/// OpenAI-compatible chat backend.
pub struct OpenAiCompatBackend {
    client: Client<OpenAIConfig>,
    provider_name: String,
    model: String,
    max_tokens: u16,
}

impl OpenAiCompatBackend {
    /// Create with explicit API key.
    pub fn new(provider: Provider, api_key: String, model: String) -> Self {
        let config = OpenAIConfig::new()
            .with_api_base(provider.base_url())
            .with_api_key(&api_key);

        let provider_name = match &provider {
            Provider::Mistral => "mistral".to_string(),
            Provider::OpenAi => "openai".to_string(),
            Provider::Custom { base_url } => format!("custom({base_url})"),
        };

        Self {
            client: Client::with_config(config),
            provider_name,
            model,
            max_tokens: 4096,
        }
    }

    /// Create from environment variable.
    pub fn from_env(provider: Provider, model: &str) -> Result<Self> {
        let env_key = provider.env_key_name();
        let api_key = std::env::var(env_key).context(format!("{env_key} not set"))?;
        Ok(Self::new(provider, api_key, model.to_string()))
    }

    /// Convenience: create a Mistral/Devstral2 backend.
    pub fn devstral(api_key: String) -> Self {
        Self::new(Provider::Mistral, api_key, "devstral-small-2505".into())
    }

    pub fn with_max_tokens(mut self, max_tokens: u16) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[async_trait]
impl AiBackend for OpenAiCompatBackend {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn capability(&self) -> BackendCapability {
        BackendCapability::Chat
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn invoke(&self, request: &AiRequest) -> Result<AiResponse> {
        let mut messages: Vec<ChatCompletionRequestMessage> = Vec::new();

        if let Some(ref sys) = request.system_prompt {
            messages.push(ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessage::from(sys.as_str()),
            ));
        }

        messages.push(ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage::from(request.prompt.as_str()),
        ));

        let max_tokens = request.max_tokens.unwrap_or(self.max_tokens as u32);

        let mut req_builder = CreateChatCompletionRequestArgs::default();
        req_builder
            .model(&self.model)
            .messages(messages)
            .max_tokens(max_tokens);

        if let Some(temp) = request.temperature {
            req_builder.temperature(temp);
        }

        let api_request = req_builder
            .build()
            .context("failed to build chat completion request")?;

        tracing::info!(
            provider = %self.provider_name,
            model = %self.model,
            prompt_len = request.prompt.len(),
            "invoking OpenAI-compatible API"
        );

        let response = self
            .client
            .chat()
            .create(api_request)
            .await
            .context("OpenAI-compatible API call failed")?;

        let content = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .unwrap_or("")
            .to_string();

        let (input_tokens, output_tokens) = response
            .usage
            .map(|u| {
                (
                    Some(u.prompt_tokens as u64),
                    Some(u.completion_tokens as u64),
                )
            })
            .unwrap_or((None, None));

        Ok(AiResponse {
            content,
            model: response.model,
            input_tokens,
            output_tokens,
            timed_out: false,
            exit_code: None,
        })
    }

    async fn health_check(&self) -> Result<()> {
        // List models as a basic connectivity check
        let _models = self
            .client
            .models()
            .list()
            .await
            .context("failed to list models — check API key and endpoint")?;
        Ok(())
    }
}
