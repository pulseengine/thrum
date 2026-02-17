use serde::Deserialize;
use std::collections::HashMap;

/// Configuration for an agent role (implementer, reviewer, planner, etc.).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentRole {
    /// Name of the AI backend to use (e.g., "opus", "sonnet", "haiku", or exact backend name).
    pub backend: String,
    /// Path to the prompt template file (relative to agents dir).
    pub prompt_template: String,
    /// Per-invocation budget in USD.
    pub budget_usd: Option<f64>,
    /// Timeout in seconds for this role's invocations.
    pub timeout_secs: Option<u64>,
}

/// Declarative backend configuration loaded from pipeline.toml `[[backends]]`.
///
/// This allows any coding agent (Claude, OpenCode, Copilot, Aider, etc.) to be
/// registered via config instead of hardcoded in the binary.
#[derive(Debug, Clone, Deserialize)]
pub struct BackendConfig {
    /// Unique name for this backend (e.g., "claude-code", "opencode", "mistral-api").
    pub name: String,
    /// Backend type: "agent" (CLI-based, can edit files) or "chat" (API-based, text only).
    #[serde(rename = "type")]
    pub backend_type: String,
    /// CLI command for agent backends (e.g., "claude", "opencode", "aider").
    pub command: Option<String>,
    /// How to pass the prompt. Use `{prompt}` as placeholder.
    /// e.g., `["-p", "{prompt}", "--output-format", "json"]`
    pub prompt_args: Option<Vec<String>>,
    /// Model identifier (e.g., "claude-opus-4-6", "devstral-small-2505").
    pub model: Option<String>,
    /// API provider: "anthropic", "openai", "mistral", or "custom".
    pub provider: Option<String>,
    /// Base URL for API backends (required for "custom" provider).
    pub base_url: Option<String>,
    /// Environment variable name holding the API key.
    pub api_key_env: Option<String>,
    /// Session timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Whether this backend is active. Disabled backends are skipped during registration.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl BackendConfig {
    /// Check if this is an agent-type (CLI) backend.
    pub fn is_agent(&self) -> bool {
        self.backend_type == "agent"
    }

    /// Check if this is a chat-type (API) backend.
    pub fn is_chat(&self) -> bool {
        self.backend_type == "chat"
    }
}

/// Collection of role definitions, loaded from pipeline.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct RolesConfig {
    pub roles: HashMap<String, AgentRole>,
}

impl RolesConfig {
    /// Get a role by name.
    pub fn get(&self, name: &str) -> Option<&AgentRole> {
        self.roles.get(name)
    }

    /// Get the implementer role, falling back to defaults.
    pub fn implementer(&self) -> AgentRole {
        self.roles.get("implementer").cloned().unwrap_or(AgentRole {
            backend: "opus".into(),
            prompt_template: "agents/implementer.md".into(),
            budget_usd: Some(6.0),
            timeout_secs: Some(600),
        })
    }

    /// Get the reviewer role, falling back to defaults.
    pub fn reviewer(&self) -> AgentRole {
        self.roles.get("reviewer").cloned().unwrap_or(AgentRole {
            backend: "sonnet".into(),
            prompt_template: "agents/reviewer.md".into(),
            budget_usd: Some(1.0),
            timeout_secs: Some(300),
        })
    }

    /// Get the planner role, falling back to defaults.
    pub fn planner(&self) -> AgentRole {
        self.roles.get("planner").cloned().unwrap_or(AgentRole {
            backend: "opus".into(),
            prompt_template: "agents/planner.md".into(),
            budget_usd: Some(1.0),
            timeout_secs: Some(300),
        })
    }

    /// Get the ci_fixer role, falling back to defaults.
    pub fn ci_fixer(&self) -> AgentRole {
        self.roles.get("ci_fixer").cloned().unwrap_or(AgentRole {
            backend: "opus".into(),
            prompt_template: "agents/ci_fixer.md".into(),
            budget_usd: Some(3.0),
            timeout_secs: Some(600),
        })
    }
}

impl Default for RolesConfig {
    fn default() -> Self {
        let mut roles = HashMap::new();
        roles.insert(
            "implementer".into(),
            AgentRole {
                backend: "opus".into(),
                prompt_template: "agents/implementer.md".into(),
                budget_usd: Some(6.0),
                timeout_secs: Some(600),
            },
        );
        roles.insert(
            "reviewer".into(),
            AgentRole {
                backend: "sonnet".into(),
                prompt_template: "agents/reviewer.md".into(),
                budget_usd: Some(1.0),
                timeout_secs: Some(300),
            },
        );
        roles.insert(
            "planner".into(),
            AgentRole {
                backend: "opus".into(),
                prompt_template: "agents/planner.md".into(),
                budget_usd: Some(1.0),
                timeout_secs: Some(300),
            },
        );
        Self { roles }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roles() {
        let config = RolesConfig::default();
        assert!(config.get("implementer").is_some());
        assert!(config.get("reviewer").is_some());
        assert!(config.get("planner").is_some());
        assert_eq!(config.implementer().backend, "opus");
        assert_eq!(config.reviewer().backend, "sonnet");
    }

    #[test]
    fn custom_role_override() {
        let mut roles = HashMap::new();
        roles.insert(
            "implementer".into(),
            AgentRole {
                backend: "haiku".into(),
                prompt_template: "agents/fast_impl.md".into(),
                budget_usd: Some(0.5),
                timeout_secs: Some(120),
            },
        );
        let config = RolesConfig { roles };
        assert_eq!(config.implementer().backend, "haiku");
        // Reviewer falls back to default since not configured
        assert_eq!(config.reviewer().backend, "sonnet");
    }

    #[test]
    fn backend_config_deserialize() {
        let toml_str = r#"
            [[backends]]
            name = "claude-code"
            type = "agent"
            command = "claude"
            prompt_args = ["-p", "{prompt}", "--output-format", "json"]
            model = "claude-opus-4-6"
            enabled = true

            [[backends]]
            name = "opencode"
            type = "agent"
            command = "opencode"
            prompt_args = ["-m", "{prompt}"]
            model = "devstral-small-2505"
            enabled = true

            [[backends]]
            name = "anthropic-api"
            type = "chat"
            provider = "anthropic"
            model = "claude-sonnet-4-5-20250929"
            api_key_env = "ANTHROPIC_API_KEY"
            enabled = true

            [[backends]]
            name = "mistral-api"
            type = "chat"
            provider = "mistral"
            model = "devstral-small-2505"
            api_key_env = "MISTRAL_API_KEY"
            enabled = false
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            backends: Vec<BackendConfig>,
        }
        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.backends.len(), 4);

        let claude = &parsed.backends[0];
        assert_eq!(claude.name, "claude-code");
        assert!(claude.is_agent());
        assert_eq!(claude.command.as_deref(), Some("claude"));
        assert!(claude.enabled);

        let opencode = &parsed.backends[1];
        assert_eq!(opencode.name, "opencode");
        assert!(opencode.is_agent());

        let anthropic = &parsed.backends[2];
        assert!(anthropic.is_chat());
        assert_eq!(anthropic.provider.as_deref(), Some("anthropic"));

        let mistral = &parsed.backends[3];
        assert!(!mistral.enabled);
    }

    #[test]
    fn backend_config_defaults() {
        let toml_str = r#"
            [[backends]]
            name = "test"
            type = "agent"
        "#;

        #[derive(Deserialize)]
        struct Wrapper {
            backends: Vec<BackendConfig>,
        }
        let parsed: Wrapper = toml::from_str(toml_str).unwrap();
        assert!(parsed.backends[0].enabled); // defaults to true
        assert!(parsed.backends[0].command.is_none());
        assert!(parsed.backends[0].model.is_none());
    }
}
