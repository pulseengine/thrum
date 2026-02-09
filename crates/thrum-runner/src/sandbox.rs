//! Sandbox execution backends for isolating agent commands.
//!
//! Fallback chain: Docker → OS-native → None (passthrough).

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Output from a sandboxed command execution.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

/// Sandbox configuration, loaded from pipeline.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    /// Which backend to use: "docker", "os-native", "none"
    pub backend: String,
    /// Docker image name (for docker backend)
    pub image: Option<String>,
    /// Memory limit in MB
    #[serde(default = "default_memory_limit")]
    pub memory_limit_mb: u64,
    /// CPU limit (number of CPUs)
    #[serde(default = "default_cpu_limit")]
    pub cpu_limit: f64,
    /// Allow network access
    #[serde(default)]
    pub network: bool,
    /// Host → container path mappings
    #[serde(default)]
    pub mount_paths: Vec<MountPath>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MountPath {
    pub host: PathBuf,
    pub container: PathBuf,
}

fn default_memory_limit() -> u64 {
    4096
}
fn default_cpu_limit() -> f64 {
    2.0
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            backend: "none".into(),
            image: None,
            memory_limit_mb: 4096,
            cpu_limit: 2.0,
            network: false,
            mount_paths: Vec::new(),
        }
    }
}

/// Trait for sandbox execution backends.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Execute a command within the sandbox.
    async fn execute(&self, cmd: &str, args: &[&str], work_dir: &Path) -> Result<ProcessOutput>;

    /// Clean up sandbox resources.
    async fn cleanup(&self) -> Result<()>;

    /// Name of this sandbox backend.
    fn name(&self) -> &str;
}

/// No-op passthrough sandbox (for development).
pub struct NoSandbox;

#[async_trait]
impl Sandbox for NoSandbox {
    async fn execute(&self, cmd: &str, args: &[&str], work_dir: &Path) -> Result<ProcessOutput> {
        let output = tokio::process::Command::new(cmd)
            .args(args)
            .current_dir(work_dir)
            .output()
            .await
            .context(format!("failed to execute: {cmd}"))?;

        Ok(ProcessOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            timed_out: false,
        })
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
    }
    fn name(&self) -> &str {
        "none"
    }
}

/// Docker-based sandbox using bollard.
pub struct DockerSandbox {
    client: bollard::Docker,
    config: SandboxConfig,
    container_id: Option<String>,
}

impl DockerSandbox {
    /// Create a new Docker sandbox (connects to local Docker daemon).
    pub async fn new(config: SandboxConfig) -> Result<Self> {
        let client = bollard::Docker::connect_with_local_defaults()
            .context("failed to connect to Docker daemon")?;

        // Verify connection
        client
            .ping()
            .await
            .context("Docker daemon not responding")?;

        Ok(Self {
            client,
            config,
            container_id: None,
        })
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn execute(&self, cmd: &str, args: &[&str], work_dir: &Path) -> Result<ProcessOutput> {
        use bollard::container::{
            Config, CreateContainerOptions, LogsOptions, StartContainerOptions,
            WaitContainerOptions,
        };
        use bollard::models::HostConfig;
        use futures_util::StreamExt;

        let image = self
            .config
            .image
            .clone()
            .unwrap_or_else(|| "ubuntu:latest".to_string());
        let mut full_cmd = vec![cmd.to_string()];
        full_cmd.extend(args.iter().map(|s| s.to_string()));

        let mut binds = vec![format!("{}:{}", work_dir.display(), work_dir.display())];
        for mount in &self.config.mount_paths {
            binds.push(format!(
                "{}:{}",
                mount.host.display(),
                mount.container.display()
            ));
        }

        let host_config = HostConfig {
            binds: Some(binds),
            memory: Some((self.config.memory_limit_mb * 1024 * 1024) as i64),
            nano_cpus: Some((self.config.cpu_limit * 1_000_000_000.0) as i64),
            network_mode: if self.config.network {
                None
            } else {
                Some("none".to_string())
            },
            ..Default::default()
        };

        let container_config: Config<String> = Config {
            image: Some(image),
            cmd: Some(full_cmd),
            working_dir: Some(work_dir.to_string_lossy().to_string()),
            host_config: Some(host_config),
            ..Default::default()
        };

        let create_opts = CreateContainerOptions {
            name: "".to_string(),
            platform: None,
        };
        let container = self
            .client
            .create_container(Some(create_opts), container_config)
            .await
            .context("failed to create container")?;

        self.client
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
            .context("failed to start container")?;

        // Wait for completion
        let mut wait_stream = self
            .client
            .wait_container(&container.id, None::<WaitContainerOptions<String>>);
        let exit_code = if let Some(result) = wait_stream.next().await {
            result.context("container wait failed")?.status_code as i32
        } else {
            -1
        };

        // Collect logs
        let log_opts = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            ..Default::default()
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut log_stream = self.client.logs(&container.id, Some(log_opts));
        while let Some(log) = log_stream.next().await {
            if let Ok(output) = log {
                match output {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        // Cleanup container
        let _ = self.client.remove_container(&container.id, None).await;

        Ok(ProcessOutput {
            stdout,
            stderr,
            exit_code,
            timed_out: false,
        })
    }

    async fn cleanup(&self) -> Result<()> {
        if let Some(ref id) = self.container_id {
            let _ = self.client.remove_container(id, None).await;
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "docker"
    }
}

/// OS-native sandbox (bubblewrap on Linux, sandbox-exec on macOS).
pub struct OsNativeSandbox {
    #[allow(dead_code)]
    config: SandboxConfig,
}

impl OsNativeSandbox {
    pub fn new(config: SandboxConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Sandbox for OsNativeSandbox {
    async fn execute(&self, cmd: &str, args: &[&str], work_dir: &Path) -> Result<ProcessOutput> {
        let output = if cfg!(target_os = "linux") {
            // Use bubblewrap on Linux
            let mut bwrap_args = vec![
                "--ro-bind",
                "/usr",
                "/usr",
                "--ro-bind",
                "/lib",
                "/lib",
                "--ro-bind",
                "/lib64",
                "/lib64",
                "--ro-bind",
                "/bin",
                "/bin",
                "--ro-bind",
                "/sbin",
                "/sbin",
                "--bind",
                work_dir.to_str().unwrap_or("."),
                work_dir.to_str().unwrap_or("."),
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--chdir",
                work_dir.to_str().unwrap_or("."),
                "--unshare-net",
                cmd,
            ];
            let args_owned: Vec<&str> = args.to_vec();
            bwrap_args.extend(args_owned);

            tokio::process::Command::new("bwrap")
                .args(&bwrap_args)
                .output()
                .await
                .context("failed to execute bwrap")?
        } else if cfg!(target_os = "macos") {
            // Use sandbox-exec on macOS with a restrictive profile
            let profile = format!(
                "(version 1)\n\
                 (deny default)\n\
                 (allow process-exec)\n\
                 (allow process-fork)\n\
                 (allow file-read*)\n\
                 (allow file-write* (subpath \"{}\"))\n\
                 (allow sysctl-read)\n\
                 (allow mach-lookup)\n",
                work_dir.display()
            );

            let mut all_args = vec!["-p".to_string(), profile, cmd.to_string()];
            all_args.extend(args.iter().map(|s| s.to_string()));

            tokio::process::Command::new("sandbox-exec")
                .args(&all_args)
                .current_dir(work_dir)
                .output()
                .await
                .context("failed to execute sandbox-exec")?
        } else {
            // Unsupported OS — fall through to regular execution
            tokio::process::Command::new(cmd)
                .args(args)
                .current_dir(work_dir)
                .output()
                .await
                .context(format!("failed to execute: {cmd}"))?
        };

        Ok(ProcessOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            timed_out: false,
        })
    }

    async fn cleanup(&self) -> Result<()> {
        Ok(())
    }
    fn name(&self) -> &str {
        "os-native"
    }
}

/// Create the appropriate sandbox based on config, with fallback chain.
///
/// Tries: Docker → OS-native → None (with warning).
pub async fn create_sandbox(config: &SandboxConfig) -> Box<dyn Sandbox> {
    match config.backend.as_str() {
        "docker" => {
            match DockerSandbox::new(config.clone()).await {
                Ok(sandbox) => {
                    tracing::info!("using Docker sandbox");
                    return Box::new(sandbox);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Docker sandbox unavailable, trying OS-native fallback");
                }
            }
            // Fallback to OS-native
            tracing::info!("falling back to OS-native sandbox");
            Box::new(OsNativeSandbox::new(config.clone()))
        }
        "os-native" => {
            tracing::info!("using OS-native sandbox");
            Box::new(OsNativeSandbox::new(config.clone()))
        }
        _ => {
            if config.backend != "none" {
                tracing::warn!(backend = %config.backend, "unknown sandbox backend, using passthrough");
            }
            tracing::info!("using passthrough (no sandbox)");
            Box::new(NoSandbox)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = SandboxConfig::default();
        assert_eq!(config.backend, "none");
        assert_eq!(config.memory_limit_mb, 4096);
        assert_eq!(config.cpu_limit, 2.0);
        assert!(!config.network);
    }

    #[tokio::test]
    async fn no_sandbox_execute() {
        let sandbox = NoSandbox;
        let result = sandbox
            .execute("echo", &["hello"], Path::new("."))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn create_sandbox_none() {
        let config = SandboxConfig::default();
        let sandbox = create_sandbox(&config).await;
        assert_eq!(sandbox.name(), "none");
    }
}
