//! AI backend abstraction for multi-provider support.
//!
//! Two categories of backends:
//! - **Agent backends** (CLI-based): Can edit files, run commands, use git.
//!   Examples: Claude Code CLI, Vibe, OpenCode.
//! - **Chat backends** (API-based): Return text responses only.
//!   Examples: Anthropic Messages API, Mistral/Devstral2 via OpenAI-compat.
//!
//! Agent backends are preferred for implementation tasks.
//! Chat backends are used for reviews, planning, and headless operation.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Capability level of a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendCapability {
    /// Full agent: can edit files, run terminal commands, use git.
    /// Invoked via CLI (e.g., `claude -p`, `vibe`, `opencode`).
    Agent,
    /// Chat only: returns text responses. No file/terminal access.
    /// Invoked via HTTP API.
    Chat,
}

/// Result from an AI backend invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiResponse {
    /// The text output from the AI.
    pub content: String,
    /// Model used (e.g., "claude-opus-4-6", "devstral-small-2505").
    pub model: String,
    /// Input tokens consumed.
    pub input_tokens: Option<u64>,
    /// Output tokens produced.
    pub output_tokens: Option<u64>,
    /// Whether the invocation timed out.
    pub timed_out: bool,
    /// Exit code (for CLI-based backends).
    pub exit_code: Option<i32>,
}

/// Configuration for an AI invocation.
#[derive(Debug, Clone)]
pub struct AiRequest {
    /// The prompt to send.
    pub prompt: String,
    /// System prompt / instructions.
    pub system_prompt: Option<String>,
    /// Working directory (for agent backends).
    pub cwd: Option<PathBuf>,
    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
    /// Temperature (0.0 - 1.0).
    pub temperature: Option<f32>,
}

impl AiRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            system_prompt: None,
            cwd: None,
            max_tokens: None,
            temperature: None,
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system_prompt = Some(system.into());
        self
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

/// Trait for all AI backends (both agent and chat).
#[async_trait]
pub trait AiBackend: Send + Sync {
    /// Human-readable name of this backend.
    fn name(&self) -> &str;

    /// What this backend can do.
    fn capability(&self) -> BackendCapability;

    /// Model identifier used by this backend.
    fn model(&self) -> &str;

    /// Invoke the AI with a request.
    async fn invoke(&self, request: &AiRequest) -> Result<AiResponse>;

    /// Check if the backend is available (e.g., API key set, CLI installed).
    async fn health_check(&self) -> Result<()>;
}

/// Registry of available backends with routing logic.
///
/// Backends can be registered programmatically or built from `[[backends]]` config
/// in pipeline.toml, enabling any coding agent to be swapped in without code changes.
pub struct BackendRegistry {
    backends: Vec<Box<dyn AiBackend>>,
}

impl BackendRegistry {
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    pub fn register(&mut self, backend: Box<dyn AiBackend>) {
        self.backends.push(backend);
    }

    /// Get the best agent backend (for implementation tasks).
    pub fn agent(&self) -> Option<&dyn AiBackend> {
        self.backends
            .iter()
            .find(|b| b.capability() == BackendCapability::Agent)
            .map(|b| b.as_ref())
    }

    /// Get the best chat backend (for reviews, planning).
    pub fn chat(&self) -> Option<&dyn AiBackend> {
        self.backends
            .iter()
            .find(|b| b.capability() == BackendCapability::Chat)
            .map(|b| b.as_ref())
    }

    /// Get a specific backend by name.
    pub fn get(&self, name: &str) -> Option<&dyn AiBackend> {
        self.backends
            .iter()
            .find(|b| b.name() == name)
            .map(|b| b.as_ref())
    }

    /// Resolve a role's backend preference to an actual registered backend.
    ///
    /// Resolution order:
    /// 1. Exact match by name (e.g., role.backend = "claude-code" → backend named "claude-code")
    /// 2. Model substring match (e.g., role.backend = "opus" → backend whose model contains "opus")
    /// 3. Capability fallback (implementer needs Agent, reviewer needs Chat)
    pub fn resolve_role(&self, role: &thrum_core::role::AgentRole) -> Option<&dyn AiBackend> {
        let query = &role.backend;

        // 1. Exact name match
        if let Some(b) = self.get(query) {
            return Some(b);
        }

        // 2. Model substring match (case-insensitive)
        let query_lower = query.to_lowercase();
        if let Some(b) = self
            .backends
            .iter()
            .find(|b| b.model().to_lowercase().contains(&query_lower))
        {
            return Some(b.as_ref());
        }

        // 3. Capability-based fallback: agent backends for "opus"/"haiku", chat for "sonnet"
        let prefer_chat = query_lower.contains("sonnet") || query_lower.contains("haiku");
        if prefer_chat {
            self.chat().or_else(|| self.agent())
        } else {
            self.agent().or_else(|| self.chat())
        }
    }

    /// List all registered backends.
    pub fn list(&self) -> Vec<(&str, BackendCapability, &str)> {
        self.backends
            .iter()
            .map(|b| (b.name(), b.capability(), b.model()))
            .collect()
    }

    /// Number of registered backends.
    pub fn len(&self) -> usize {
        self.backends.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }
}

impl Default for BackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `BackendRegistry` from declarative `[[backends]]` config entries.
///
/// This is the key to backend-agnostic operation: any coding agent that accepts
/// a prompt and returns output can be configured without code changes.
pub fn build_registry_from_config(
    configs: &[thrum_core::role::BackendConfig],
    default_cwd: &std::path::Path,
) -> Result<BackendRegistry> {
    let mut registry = BackendRegistry::new();

    for cfg in configs {
        if !cfg.enabled {
            tracing::debug!(name = %cfg.name, "skipping disabled backend");
            continue;
        }

        let timeout = std::time::Duration::from_secs(cfg.timeout_secs.unwrap_or(600));

        match cfg.backend_type.as_str() {
            "agent" => {
                // Special case: "claude" command uses the dedicated ClaudeCliBackend
                // for its JSON output parsing. Everything else uses CliAgentBackend.
                if cfg.command.as_deref() == Some("claude") {
                    let mut backend =
                        crate::claude::ClaudeCliBackend::new(default_cwd.to_path_buf());
                    backend.timeout = timeout;
                    backend.skip_permissions = true; // Required for non-interactive automation
                    registry.register(Box::new(backend));
                } else if let Some(ref command) = cfg.command {
                    let prompt_args = cfg
                        .prompt_args
                        .clone()
                        .unwrap_or_else(|| vec!["-m".into(), "{prompt}".into()]);
                    let backend = crate::cli_agent::CliAgentBackend {
                        name: cfg.name.clone(),
                        command: command.clone(),
                        prompt_args,
                        model_name: cfg.model.clone().unwrap_or_else(|| "unknown".into()),
                        default_cwd: default_cwd.to_path_buf(),
                        timeout,
                    };
                    registry.register(Box::new(backend));
                } else {
                    tracing::warn!(name = %cfg.name, "agent backend missing 'command' field, skipping");
                }
            }
            "chat" => {
                let provider = cfg.provider.as_deref().unwrap_or("anthropic");
                let model = cfg.model.as_deref().unwrap_or("claude-sonnet-4-5-20250929");
                let api_key_env = cfg.api_key_env.as_deref().unwrap_or(match provider {
                    "anthropic" => "ANTHROPIC_API_KEY",
                    "mistral" => "MISTRAL_API_KEY",
                    "openai" => "OPENAI_API_KEY",
                    _ => "OPENAI_API_KEY",
                });

                match std::env::var(api_key_env) {
                    Ok(api_key) => match provider {
                        "anthropic" => {
                            let backend = crate::anthropic::AnthropicApiBackend::new(
                                api_key,
                                model.to_string(),
                            );
                            registry.register(Box::new(backend));
                        }
                        "mistral" => {
                            let backend = crate::openai_compat::OpenAiCompatBackend::new(
                                crate::openai_compat::Provider::Mistral,
                                api_key,
                                model.to_string(),
                            );
                            registry.register(Box::new(backend));
                        }
                        "openai" => {
                            let backend = crate::openai_compat::OpenAiCompatBackend::new(
                                crate::openai_compat::Provider::OpenAi,
                                api_key,
                                model.to_string(),
                            );
                            registry.register(Box::new(backend));
                        }
                        _ => {
                            let base_url = cfg
                                .base_url
                                .clone()
                                .unwrap_or_else(|| "https://api.openai.com/v1".into());
                            let backend = crate::openai_compat::OpenAiCompatBackend::new(
                                crate::openai_compat::Provider::Custom { base_url },
                                api_key,
                                model.to_string(),
                            );
                            registry.register(Box::new(backend));
                        }
                    },
                    Err(_) => {
                        tracing::debug!(
                            name = %cfg.name,
                            env = api_key_env,
                            "chat backend API key not set, skipping"
                        );
                    }
                }
            }
            other => {
                tracing::warn!(name = %cfg.name, backend_type = other, "unknown backend type, skipping");
            }
        }
    }

    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thrum_core::role::AgentRole;

    /// A mock backend for testing routing logic without real CLI/API calls.
    struct MockBackend {
        mock_name: String,
        mock_model: String,
        mock_capability: BackendCapability,
    }

    impl MockBackend {
        fn agent(name: &str, model: &str) -> Box<dyn AiBackend> {
            Box::new(Self {
                mock_name: name.into(),
                mock_model: model.into(),
                mock_capability: BackendCapability::Agent,
            })
        }

        fn chat(name: &str, model: &str) -> Box<dyn AiBackend> {
            Box::new(Self {
                mock_name: name.into(),
                mock_model: model.into(),
                mock_capability: BackendCapability::Chat,
            })
        }
    }

    #[async_trait]
    impl AiBackend for MockBackend {
        fn name(&self) -> &str {
            &self.mock_name
        }
        fn capability(&self) -> BackendCapability {
            self.mock_capability
        }
        fn model(&self) -> &str {
            &self.mock_model
        }
        async fn invoke(&self, _request: &AiRequest) -> Result<AiResponse> {
            Ok(AiResponse {
                content: format!("mock response from {}", self.mock_name),
                model: self.mock_model.clone(),
                input_tokens: None,
                output_tokens: None,
                timed_out: false,
                exit_code: None,
            })
        }
        async fn health_check(&self) -> Result<()> {
            Ok(())
        }
    }

    fn make_role(backend: &str) -> AgentRole {
        AgentRole {
            backend: backend.into(),
            prompt_template: "agents/test.md".into(),
            budget_usd: Some(1.0),
            timeout_secs: Some(60),
        }
    }

    /// Build a registry with multiple backends simulating a real multi-provider setup.
    fn multi_provider_registry() -> BackendRegistry {
        let mut reg = BackendRegistry::new();
        reg.register(MockBackend::agent("claude-code", "claude-opus-4-6"));
        reg.register(MockBackend::agent("opencode", "devstral-small-2505"));
        reg.register(MockBackend::chat(
            "anthropic-api",
            "claude-sonnet-4-5-20250929",
        ));
        reg.register(MockBackend::chat("mistral-api", "devstral-small-2505"));
        reg
    }

    #[test]
    fn resolve_role_exact_name() {
        let reg = multi_provider_registry();
        let role = make_role("opencode");
        let backend = reg.resolve_role(&role).unwrap();
        assert_eq!(backend.name(), "opencode");
    }

    #[test]
    fn resolve_role_model_substring() {
        let reg = multi_provider_registry();
        // "opus" should match "claude-opus-4-6"
        let role = make_role("opus");
        let backend = reg.resolve_role(&role).unwrap();
        assert_eq!(backend.name(), "claude-code");
    }

    #[test]
    fn resolve_role_sonnet_prefers_chat() {
        let reg = multi_provider_registry();
        let role = make_role("sonnet");
        let backend = reg.resolve_role(&role).unwrap();
        // "sonnet" substring matches the chat backend's model
        assert_eq!(backend.name(), "anthropic-api");
    }

    #[test]
    fn resolve_role_unknown_falls_back_to_agent() {
        let reg = multi_provider_registry();
        let role = make_role("some-unknown-backend");
        let backend = reg.resolve_role(&role).unwrap();
        // Falls back to first agent
        assert_eq!(backend.capability(), BackendCapability::Agent);
    }

    #[test]
    fn registry_basic_ops() {
        let reg = multi_provider_registry();
        assert_eq!(reg.len(), 4);
        assert!(!reg.is_empty());
        assert!(reg.agent().is_some());
        assert!(reg.chat().is_some());
        assert!(reg.get("mistral-api").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn config_driven_agent_backends() {
        let configs = vec![
            thrum_core::role::BackendConfig {
                name: "claude-code".into(),
                backend_type: "agent".into(),
                command: Some("claude".into()),
                prompt_args: Some(vec!["-p".into(), "{prompt}".into()]),
                model: Some("claude-opus-4-6".into()),
                provider: None,
                base_url: None,
                api_key_env: None,
                timeout_secs: Some(300),
                enabled: true,
            },
            thrum_core::role::BackendConfig {
                name: "opencode".into(),
                backend_type: "agent".into(),
                command: Some("opencode".into()),
                prompt_args: None,
                model: Some("devstral-small-2505".into()),
                provider: None,
                base_url: None,
                api_key_env: None,
                timeout_secs: None,
                enabled: true,
            },
            thrum_core::role::BackendConfig {
                name: "disabled-agent".into(),
                backend_type: "agent".into(),
                command: Some("should-not-appear".into()),
                prompt_args: None,
                model: None,
                provider: None,
                base_url: None,
                api_key_env: None,
                timeout_secs: None,
                enabled: false,
            },
        ];

        let cwd = std::env::temp_dir();
        let registry = build_registry_from_config(&configs, &cwd).unwrap();

        // Two enabled agents registered (disabled one skipped)
        assert_eq!(registry.len(), 2);
        assert!(registry.get("claude-code").is_some() || registry.agent().is_some());
        // Disabled backend should not appear
        assert!(registry.get("disabled-agent").is_none());
    }

    #[test]
    fn config_driven_chat_backends_skip_without_key() {
        // Chat backends without API keys should be silently skipped
        let configs = vec![thrum_core::role::BackendConfig {
            name: "no-key-api".into(),
            backend_type: "chat".into(),
            command: None,
            prompt_args: None,
            model: Some("gpt-4o".into()),
            provider: Some("openai".into()),
            base_url: None,
            api_key_env: Some("NONEXISTENT_API_KEY_FOR_TEST".into()),
            timeout_secs: None,
            enabled: true,
        }];

        let cwd = std::env::temp_dir();
        let registry = build_registry_from_config(&configs, &cwd).unwrap();
        assert_eq!(registry.len(), 0); // No backends registered without API key
    }

    #[test]
    fn empty_config_produces_empty_registry() {
        let cwd = std::env::temp_dir();
        let registry = build_registry_from_config(&[], &cwd).unwrap();
        assert!(registry.is_empty());
    }

    /// Proves the complete swappability story: roles resolve correctly
    /// when the underlying backends change from Claude to OpenCode/Copilot.
    #[test]
    fn backend_swap_scenario() {
        // Scenario: User switches from Claude to OpenCode as primary agent
        let mut reg = BackendRegistry::new();
        reg.register(MockBackend::agent("opencode", "devstral-small-2505"));
        reg.register(MockBackend::chat("mistral-api", "devstral-small-2505"));

        let implementer = make_role("devstral"); // model substring

        let impl_backend = reg.resolve_role(&implementer).unwrap();
        // "devstral" matches both backends. resolve_role prefers agent for non-sonnet/haiku.
        assert_eq!(impl_backend.capability(), BackendCapability::Agent);
        assert_eq!(impl_backend.name(), "opencode");

        // For reviewer, if we want to explicitly use chat, reference by name
        let reviewer_role = make_role("mistral-api");
        let rev_backend = reg.resolve_role(&reviewer_role).unwrap();
        assert_eq!(rev_backend.name(), "mistral-api");
        assert_eq!(rev_backend.capability(), BackendCapability::Chat);
    }
}
