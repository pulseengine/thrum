use crate::event_bus::EventBus;
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use thrum_core::event::OutputStream;
use tokio::io::{AsyncBufReadExt, BufReader};
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

/// Run a shell command with a timeout (non-streaming, original behavior).
pub async fn run_cmd(cmd: &str, cwd: &Path, timeout: Duration) -> Result<SubprocessOutput> {
    tracing::debug!(cmd, ?cwd, ?timeout, "spawning subprocess");

    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        // Allow Claude CLI subprocess to run inside a parent Claude session.
        .env_remove("CLAUDECODE")
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

/// Callback for streaming subprocess output lines.
///
/// Used to emit events as output arrives without coupling subprocess
/// execution to a specific event kind (agent output vs gate output).
pub type LineCallback = Box<dyn Fn(OutputStream, &str) + Send + Sync>;

/// Run a shell command with streaming output.
///
/// Like `run_cmd`, but reads stdout/stderr line-by-line as they arrive,
/// calling `on_line` for each line. Also emits events through the
/// `EventBus` if a `line_callback` is provided.
///
/// The full output is still collected and returned in `SubprocessOutput`
/// so callers can parse the final result (e.g., JSON from Claude CLI).
pub async fn run_cmd_streaming(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
    event_bus: &EventBus,
    line_callback: LineCallback,
) -> Result<SubprocessOutput> {
    tracing::debug!(cmd, ?cwd, ?timeout, "spawning streaming subprocess");

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        // Allow Claude CLI subprocess to run inside a parent Claude session.
        .env_remove("CLAUDECODE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(format!("failed to spawn: {cmd}"))?;

    let stdout = child.stdout.take().context("failed to capture stdout")?;
    let stderr = child.stderr.take().context("failed to capture stderr")?;

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    let line_callback = std::sync::Arc::new(line_callback);

    // Read both streams concurrently, collecting output and emitting events
    let read_future = async {
        loop {
            tokio::select! {
                line = stdout_reader.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            line_callback(OutputStream::Stdout, &line);
                            stdout_buf.push_str(&line);
                            stdout_buf.push('\n');
                        }
                        Ok(None) => {
                            // stdout closed, drain stderr then break
                            while let Ok(Some(line)) = stderr_reader.next_line().await {
                                line_callback(OutputStream::Stderr, &line);
                                stderr_buf.push_str(&line);
                                stderr_buf.push('\n');
                            }
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "error reading stdout");
                            break;
                        }
                    }
                }
                line = stderr_reader.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            line_callback(OutputStream::Stderr, &line);
                            stderr_buf.push_str(&line);
                            stderr_buf.push('\n');
                        }
                        Ok(None) => {
                            // stderr closed, drain stdout then break
                            while let Ok(Some(line)) = stdout_reader.next_line().await {
                                line_callback(OutputStream::Stdout, &line);
                                stdout_buf.push_str(&line);
                                stdout_buf.push('\n');
                            }
                            break;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "error reading stderr");
                            break;
                        }
                    }
                }
            }
        }

        // Wait for the process to exit
        child.wait().await
    };

    // Use the EventBus reference to keep it alive (needed for the type system)
    let _ = event_bus;

    match tokio::time::timeout(timeout, read_future).await {
        Ok(Ok(status)) => {
            let result = SubprocessOutput {
                stdout: stdout_buf,
                stderr: stderr_buf,
                exit_code: status.code().unwrap_or(-1),
                timed_out: false,
            };
            tracing::debug!(
                exit_code = result.exit_code,
                stdout_len = result.stdout.len(),
                stderr_len = result.stderr.len(),
                "streaming subprocess completed"
            );
            Ok(result)
        }
        Ok(Err(e)) => Err(e).context(format!("subprocess failed: {cmd}")),
        Err(_) => {
            tracing::warn!(cmd, ?timeout, "streaming subprocess timed out");
            // Kill the child on timeout
            let _ = child.kill().await;
            Ok(SubprocessOutput {
                stdout: stdout_buf,
                stderr: stderr_buf,
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
