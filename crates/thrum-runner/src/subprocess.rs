use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// Output from a subprocess execution.
#[derive(Debug, Clone)]
pub struct SubprocessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

impl SubprocessOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }
}

/// Run a shell command with a timeout.
pub async fn run_cmd(cmd: &str, cwd: &Path, timeout: Duration) -> Result<SubprocessOutput> {
    tracing::debug!(cmd, ?cwd, ?timeout, "spawning subprocess");

    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(format!("failed to spawn: {cmd}"))?;

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let result = SubprocessOutput {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code: output.status.code().unwrap_or(-1),
                timed_out: false,
            };
            tracing::debug!(
                exit_code = result.exit_code,
                stdout_len = result.stdout.len(),
                "subprocess completed"
            );
            Ok(result)
        }
        Ok(Err(e)) => Err(e).context(format!("subprocess failed: {cmd}")),
        Err(_) => {
            tracing::warn!(cmd, ?timeout, "subprocess timed out");
            Ok(SubprocessOutput {
                stdout: String::new(),
                stderr: format!("Process timed out after {timeout:?}"),
                exit_code: -1,
                timed_out: true,
            })
        }
    }
}

/// Run a command and return just stdout, failing on non-zero exit.
pub async fn run_cmd_stdout(cmd: &str, cwd: &Path, timeout: Duration) -> Result<String> {
    let output = run_cmd(cmd, cwd, timeout).await?;
    if !output.success() {
        anyhow::bail!(
            "command failed (exit {}): {}\nstderr: {}",
            output.exit_code,
            cmd,
            output.stderr
        );
    }
    Ok(output.stdout)
}
